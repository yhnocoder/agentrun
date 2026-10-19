use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde_json::Value;
use toml_writer::{ToTomlValue, TomlStringBuilder};

use super::{
    Adapter, Failure, Invocation, Launch, PrivateDir, Version, detail_head, reject_limits,
};
use crate::cli::{NetworkMode, RunArgs, Runtime};
use crate::image::write_copies;
use crate::json::{first_line, optional_string, string};
use crate::network::{HostRule, proxy_environment};
use crate::output::{Record, SandboxKind, TokenCounts};
use crate::sandbox::{BWRAP_PREFIX, Hidden, hidden, start_failure};
use crate::session::{CODEX_AUTH_VARIABLE, Session, home_placeholder};
use crate::transcript::{Transcript, copy_into, find_file_within};

pub(crate) const MIN_VERSION: Version = Version(0, 159, 2);
pub(crate) const HOME_VARIABLE: &str = "CODEX_HOME";
pub(crate) const HOME_SUBDIR: &str = ".codex";
const HOME_SUFFIX: &str = "-codex";
const HOME_LABEL: &str = "codex home";
const SESSIONS_SUBDIR: &str = "sessions";
const RESUME_COMMAND: &str = "resume";
pub(crate) const LOGIN_FILE: &str = "auth.json";
const GLOB_CHARACTERS: [char; 6] = ['*', '?', '[', ']', '{', '}'];
const HISTORY_ENTRIES: [&str; 2] = ["sessions", "history.jsonl"];
pub(crate) const SERVICE_HOSTS: [&str; 4] = [
    "chatgpt.com",
    "ab.chatgpt.com",
    "auth.openai.com",
    "api.openai.com",
];
pub(crate) const SERVICE_HOST: &str = SERVICE_HOSTS[0];
const FIXED_ARGS: [&str; 6] = [
    "exec",
    "--json",
    "--skip-git-repo-check",
    "--ignore-user-config",
    "--ignore-rules",
    "-C",
];
const PROFILE: &str = "agentrun";
const DENIED_ACCESS: &str = "\"deny\"";
const FULL_ACCESS_PROFILE: &str = "\":danger-full-access\"";
const SETTINGS: [&str; 5] = [
    "approval_policy=\"never\"",
    "project_doc_max_bytes=0",
    "skills.include_instructions=false",
    "skills.bundled.enabled=false",
    "allow_login_shell=false",
];
const DISABLED_FEATURES: [&str; 16] = [
    "apps",
    "plugins",
    "remote_plugin",
    "plugin_sharing",
    "tool_suggest",
    "hooks",
    "skill_mcp_dependency_install",
    "image_generation",
    "goals",
    "memories",
    "shell_snapshot",
    "computer_use",
    "browser_use",
    "browser_use_external",
    "in_app_browser",
    "daemon_auto_start",
];
const NO_SUBAGENTS_ARGS: [&str; 6] = [
    "-c",
    "agents.enabled=false",
    "--disable",
    "multi_agent",
    "--disable",
    "multi_agent_v2",
];
const IMAGE_LIST_SEPARATOR: char = ',';
const NO_TURN_DETAIL: &str = "codex produced no turn.completed";

#[derive(Default)]
pub struct Codex {
    cwd: PathBuf,
    model: Option<String>,
    sandboxed: bool,
    sandbox_failure: Option<String>,
    turn_completed: bool,
    turn_failed: Option<String>,
    last_error: Option<String>,
    home: PathBuf,
    saving: bool,
    session_id: Option<String>,
    started_items: HashSet<String>,
}

impl Codex {
    pub fn new() -> Codex {
        Codex::default()
    }

    fn item_started(&mut self, item: &Value) -> Vec<Record> {
        let Some(start) = self.tool_start(item) else {
            return Vec::new();
        };
        if !self.started_items.insert(string(&item["id"])) {
            return Vec::new();
        }
        vec![start]
    }

    fn item_completed(&mut self, item: &Value) -> Vec<Record> {
        match item["type"].as_str() {
            Some("command_execution") => self.note_sandbox_failure(&item["aggregated_output"]),
            Some("agent_message") => {
                return vec![Record::Text {
                    parent: None,
                    text: string(&item["text"]),
                }];
            }
            _ => {}
        }
        let Some(start) = self.tool_start(item) else {
            return Vec::new();
        };
        let id = string(&item["id"]);
        let end = Record::ToolEnd {
            id: id.clone(),
            denied: false,
            error: item_error(item),
        };
        if self.started_items.remove(&id) {
            vec![end]
        } else {
            vec![start, end]
        }
    }

    fn tool_start(&self, item: &Value) -> Option<Record> {
        let summary = match item["type"].as_str() {
            Some("command_execution") => {
                format!("shell: {}", first_line(&string(&item["command"])))
            }
            Some("file_change") => format!("patch: {}", self.changed_paths(&item["changes"])),
            Some("mcp_tool_call") => {
                format!("mcp: {}.{}", string(&item["server"]), string(&item["tool"]))
            }
            _ => return None,
        };
        Some(Record::ToolStart {
            id: string(&item["id"]),
            parent: None,
            name: string(&item["type"]),
            summary,
        })
    }

    fn note_sandbox_failure(&mut self, output: &Value) {
        if !self.sandboxed || self.sandbox_failure.is_some() {
            return;
        }
        if let Some(reason) =
            first_line(output.as_str().unwrap_or_default()).strip_prefix(BWRAP_PREFIX)
        {
            self.sandbox_failure = Some(start_failure(reason));
        }
    }

    fn changed_paths(&self, changes: &Value) -> String {
        changes
            .as_array()
            .map(|changes| {
                changes
                    .iter()
                    .filter_map(|change| change["path"].as_str())
                    .map(|path| {
                        Path::new(path)
                            .strip_prefix(&self.cwd)
                            .map(|relative| relative.to_string_lossy().into_owned())
                            .unwrap_or_else(|_| path.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    }
}

impl Adapter for Codex {
    fn runtime(&self) -> Runtime {
        Runtime::Codex
    }

    fn check_args(&self, args: &RunArgs) -> Result<(), String> {
        reject_limits(Runtime::Codex, args)?;
        for given in &args.deny_read {
            let absolute = std::path::absolute(given).unwrap_or_else(|_| given.clone());
            let real = std::fs::canonicalize(&absolute).unwrap_or(absolute);
            if real.to_string_lossy().contains(GLOB_CHARACTERS) {
                let resolved = if &real == given {
                    String::new()
                } else {
                    format!(" (resolves to {})", real.display())
                };
                return Err(format!(
                    "--deny-read {}{resolved}: codex cannot deny a path that contains * ? [ ] {{ }}",
                    given.display()
                ));
            }
        }
        Ok(())
    }

    fn launch(&mut self, executable: &Path, invocation: &Invocation) -> Result<Launch, String> {
        let login = login_file(&invocation.session, &invocation.cwd);
        check_login(&invocation.session, &login)?;
        self.cwd = invocation.cwd.clone();
        self.model = invocation.args.model.clone();
        self.sandboxed = invocation.sandbox.runs();
        let home = home_path(&invocation.tempdir);
        self.home = home.clone();
        self.saving = invocation.args.session_dir.is_some();
        self.session_id = invocation.resume.as_ref().map(|saved| saved.id.clone());
        let network = invocation.args.network;
        let mut argv = vec![executable.to_string_lossy().into_owned()];
        argv.extend(FIXED_ARGS.iter().map(|arg| arg.to_string()));
        argv.push(invocation.cwd.to_string_lossy().into_owned());
        for copy in write_copies(
            &invocation.images,
            &invocation.tempdir,
            invocation.args.dry_run,
        )? {
            let copy = copy.to_string_lossy().into_owned();
            if copy.contains(IMAGE_LIST_SEPARATOR) {
                return Err(format!(
                    "cannot give images to codex: the path {copy} contains '{IMAGE_LIST_SEPARATOR}', which codex reads as a list separator"
                ));
            }
            argv.push("--image".to_string());
            argv.push(copy);
        }
        if invocation.sandbox.runs() {
            let tempdir = if invocation.args.dry_run {
                invocation.tempdir.clone()
            } else {
                std::fs::canonicalize(&invocation.tempdir).map_err(|error| {
                    format!(
                        "cannot resolve the session temporary directory {}: {error}",
                        invocation.tempdir.display()
                    )
                })?
            };
            argv.extend(config(format!("default_permissions={PROFILE}")));
            let mut hidden_reads = hidden(&invocation.denied_reads);
            let real_login = std::fs::canonicalize(&login).unwrap_or_else(|_| login.clone());
            if hidden_reads
                .iter()
                .any(|entry| real_login.starts_with(&entry.path))
            {
                hidden_reads.push(Hidden {
                    path: home_path(&tempdir).join(LOGIN_FILE),
                    dir: false,
                });
            }
            argv.extend(sandbox_permissions(
                &tempdir,
                &hidden_reads,
                network,
                &invocation.allow_hosts,
            ));
        } else {
            argv.extend(config(format!("default_permissions={FULL_ACCESS_PROFILE}")));
        }
        argv.extend(config(SETTINGS[0].to_string()));
        let web_search = if network == NetworkMode::Full {
            "live"
        } else {
            "disabled"
        };
        argv.extend(config(format!("web_search=\"{web_search}\"")));
        for setting in &SETTINGS[1..] {
            argv.extend(config(setting.to_string()));
        }
        for feature in DISABLED_FEATURES {
            argv.push("--disable".to_string());
            argv.push(feature.to_string());
        }
        if invocation.args.no_subagents {
            argv.extend(NO_SUBAGENTS_ARGS.iter().map(|arg| arg.to_string()));
        }
        if let Some(model) = &invocation.args.model {
            argv.push("-m".to_string());
            argv.push(model.clone());
        }
        if let Some(effort) = &invocation.args.effort {
            argv.extend(config(format!(
                "model_reasoning_effort={}",
                toml_string(effort)
            )));
        }
        argv.extend(invocation.args.runtime_args.iter().cloned());
        if let Some(saved) = &invocation.resume {
            argv.push(RESUME_COMMAND.to_string());
            argv.push(saved.id.clone());
        }
        argv.push("-".to_string());
        let mut env = vec![(OsString::from(HOME_VARIABLE), home.clone().into_os_string())];
        if let Some(proxy) = &invocation.proxy {
            env.extend(proxy_environment(&proxy.port_text()));
        }
        let private_dirs = if invocation.args.dry_run {
            Vec::new()
        } else {
            create_home(&home, &login)?;
            if let Some(saved) = &invocation.resume {
                copy_into(&saved.file, &home.join(SESSIONS_SUBDIR)).inspect_err(|_| {
                    let _ = std::fs::remove_dir_all(&home);
                })?;
            }
            vec![PrivateDir {
                label: HOME_LABEL,
                path: home,
            }]
        };
        Ok(Launch {
            argv,
            stdin: invocation.prompt.clone().into_bytes(),
            env,
            wrapped: SandboxKind::None,
            private_dirs,
        })
    }

    fn echoes_prompt(&self) -> bool {
        false
    }

    fn translate(&mut self, line: &Value) -> Vec<Record> {
        match line["type"].as_str() {
            Some("thread.started") => {
                if self.session_id.is_none() {
                    self.session_id = optional_string(&line["thread_id"]);
                }
                Vec::new()
            }
            Some("item.started") => self.item_started(&line["item"]),
            Some("item.completed") => self.item_completed(&line["item"]),
            Some("turn.completed") => {
                self.turn_completed = true;
                vec![Record::Usage {
                    parent: None,
                    model: self.model.clone(),
                    counts: counts(&line["usage"]),
                }]
            }
            Some("turn.failed") => {
                self.turn_failed = Some(string(&line["error"]["message"]));
                Vec::new()
            }
            Some("error") => {
                self.last_error = Some(string(&line["message"]));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn after_exit(&mut self) -> Vec<Record> {
        Vec::new()
    }

    fn failure(&self, exit_code: Option<i32>) -> Option<Failure> {
        if let Some(detail) = &self.sandbox_failure {
            return Some(Failure::Message(detail.clone()));
        }
        if let Some(message) = &self.turn_failed {
            return Some(Failure::from_detail(detail_head(message)));
        }
        if let (false, Some(message)) = (self.turn_completed, &self.last_error) {
            return Some(Failure::from_detail(detail_head(message)));
        }
        if exit_code != Some(0) {
            return Some(Failure::Unexplained);
        }
        (!self.turn_completed).then(|| Failure::Message(NO_TURN_DETAIL.to_string()))
    }

    fn transcript(&self) -> Option<Transcript> {
        if !self.saving {
            return None;
        }
        let id = self.session_id.clone()?;
        let file = find_file_within(&self.home.join(SESSIONS_SUBDIR), &id)?;
        Some(Transcript {
            id,
            entries: vec![file],
            emptied_dir: None,
        })
    }
}

pub(crate) fn user_home(session: &Session, cwd: &Path) -> Option<PathBuf> {
    session.runtime_dir(cwd, HOME_VARIABLE, HOME_SUBDIR)
}

pub(crate) fn history_paths(session: &Session, cwd: &Path) -> Vec<PathBuf> {
    user_home(session, cwd)
        .into_iter()
        .flat_map(|home| HISTORY_ENTRIES.map(|entry| home.join(entry)))
        .collect()
}

pub(crate) fn login_file(session: &Session, cwd: &Path) -> PathBuf {
    user_home(session, cwd)
        .unwrap_or_else(|| home_placeholder(HOME_SUBDIR))
        .join(LOGIN_FILE)
}

pub(crate) fn check_login(session: &Session, login: &Path) -> Result<(), String> {
    let provided = session
        .codex_auth
        .as_ref()
        .is_some_and(|value| !value.is_empty());
    if provided || login.exists() {
        Ok(())
    } else {
        Err(format!(
            "codex login file {} not found. Run codex login, or pass its content in {CODEX_AUTH_VARIABLE}",
            login.display()
        ))
    }
}

pub(crate) fn home_path(tempdir: &Path) -> PathBuf {
    let mut name = tempdir.as_os_str().to_owned();
    name.push(HOME_SUFFIX);
    PathBuf::from(name)
}

pub(crate) fn create_home(home: &Path, login: &Path) -> Result<(), String> {
    DirBuilder::new()
        .mode(0o700)
        .create(home)
        .and_then(|()| {
            std::os::unix::fs::symlink(login, home.join(LOGIN_FILE)).inspect_err(|_| {
                let _ = std::fs::remove_dir_all(home);
            })
        })
        .map_err(|error| {
            format!(
                "cannot create the codex home directory {}: {error}",
                home.display()
            )
        })
}

pub(crate) fn sandbox_permissions(
    tempdir: &Path,
    hidden_reads: &[Hidden],
    network: NetworkMode,
    allow_hosts: &[HostRule],
) -> Vec<String> {
    let mut permissions = config(filesystem_setting(tempdir, hidden_reads)).to_vec();
    permissions.extend(network_settings(network, allow_hosts));
    permissions
}

pub(crate) fn sandbox_command(
    tempdir: &Path,
    hidden_reads: &[Hidden],
    cwd: &Path,
    network: NetworkMode,
    allow_hosts: &[HostRule],
) -> Vec<String> {
    let mut command = ["sandbox", "-P", PROFILE].map(str::to_string).to_vec();
    command.extend(sandbox_permissions(
        tempdir,
        hidden_reads,
        network,
        allow_hosts,
    ));
    command.push("-C".to_string());
    command.push(cwd.to_string_lossy().into_owned());
    command.push("--".to_string());
    command
}

fn config(value: String) -> [String; 2] {
    ["-c".to_string(), value]
}

fn filesystem_setting(tempdir: &Path, hidden: &[Hidden]) -> String {
    let denied: String = hidden
        .iter()
        .map(|entry| {
            format!(
                ", {}={DENIED_ACCESS}",
                toml_string(&entry.path.to_string_lossy())
            )
        })
        .collect();
    format!(
        "permissions.{PROFILE}.filesystem={{\":root\"=\"read\", \":workspace_roots\"={{\".\"=\"write\"}}, {}=\"write\"{denied}}}",
        toml_string(&tempdir.to_string_lossy())
    )
}

fn network_settings(network: NetworkMode, allow_hosts: &[HostRule]) -> Vec<String> {
    match network {
        NetworkMode::None => {
            config(format!("permissions.{PROFILE}.network.enabled=false")).to_vec()
        }
        NetworkMode::Full => config(format!("permissions.{PROFILE}.network.enabled=true")).to_vec(),
        NetworkMode::Custom => {
            let mut settings = ["--enable", "network_proxy"].map(str::to_string).to_vec();
            settings.extend(config(format!(
                "permissions.{PROFILE}.network={{enabled=true, enable_socks5=false, enable_socks5_udp=false, allow_upstream_proxy=true, domains={{{}}}}}",
                domains(allow_hosts)
            )));
            settings
        }
    }
}

fn domains(allow_hosts: &[HostRule]) -> String {
    let mut hosts: Vec<String> = Vec::new();
    for rule in allow_hosts {
        let pattern = rule.pattern();
        if !hosts.contains(&pattern) {
            hosts.push(pattern);
        }
    }
    hosts
        .iter()
        .map(|host| format!("{}=\"allow\"", toml_string(host)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn toml_string(text: &str) -> String {
    TomlStringBuilder::new(text).as_basic().to_toml_value()
}

fn item_error(item: &Value) -> Option<bool> {
    let exit_code = item["exit_code"].as_i64();
    let status = item["status"].as_str();
    if exit_code.is_some_and(|code| code != 0) || status == Some("failed") {
        Some(true)
    } else if status == Some("completed") || (status.is_none() && exit_code == Some(0)) {
        Some(false)
    } else {
        None
    }
}

fn counts(usage: &Value) -> TokenCounts {
    let field = |name: &str| usage[name].as_u64().unwrap_or(0);
    let cached = field("cached_input_tokens");
    TokenCounts {
        input_tokens: Some(field("input_tokens").saturating_sub(cached)),
        output_tokens: Some(field("output_tokens")),
        cache_read_tokens: Some(cached),
        cache_write_tokens: Some(field("cache_write_input_tokens")),
        cost_usd: None,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::cli::{SandboxMode, parse_run};
    use crate::image::{Image, ImageKind};
    use crate::network::{self, ProxyEndpoint};
    use crate::sandbox::{Sandbox, Wrapper};

    struct Setup {
        root: TempDir,
    }

    impl Setup {
        fn new() -> Setup {
            let setup = Setup {
                root: tempfile::tempdir().unwrap(),
            };
            std::fs::create_dir(setup.work()).unwrap();
            std::fs::create_dir(setup.tempdir()).unwrap();
            std::fs::create_dir(setup.user_home()).unwrap();
            std::fs::write(setup.login(), "{}").unwrap();
            setup
        }

        fn work(&self) -> PathBuf {
            self.root.path().join("work")
        }

        fn user_home(&self) -> PathBuf {
            self.root.path().join("codex-user")
        }

        fn login(&self) -> PathBuf {
            self.user_home().join("auth.json")
        }

        fn tempdir(&self) -> PathBuf {
            self.root.path().join("agentrun-session")
        }

        fn invocation(&self, sandboxed: bool, extra: &[&str]) -> Invocation {
            let mut args = vec!["agentrun", "codex", "--prompt", "hi"];
            args.extend(extra);
            let (runtime, args) = parse_run(&args);
            let allow_hosts = network::check_usage(args.network, &args.allow_host).unwrap();
            let proxy = (sandboxed && args.network == NetworkMode::Custom).then(|| ProxyEndpoint {
                port: Some(4321),
                socket: self.tempdir().join("proxy.sock"),
            });
            let dry_run = args.dry_run;
            let env = vec![(
                OsString::from("CODEX_HOME"),
                self.user_home().into_os_string(),
            )];
            let wrapper = if sandboxed {
                Wrapper::Codex
            } else {
                Wrapper::None
            };
            Invocation {
                sandbox: Sandbox::new(SandboxMode::On, wrapper),
                tempdir: if dry_run {
                    PathBuf::from("<tempdir>")
                } else {
                    self.tempdir()
                },
                allow_hosts,
                proxy,
                ..Invocation::new(
                    runtime,
                    args,
                    self.work(),
                    "hi".to_string(),
                    Session::assemble(runtime, &env, &[], &[], &[]),
                )
            }
        }
    }

    fn launch(setup: &Setup, sandboxed: bool, extra: &[&str]) -> Launch {
        let _ = std::fs::remove_dir_all(home_path(&setup.tempdir()));
        Codex::new()
            .launch(
                Path::new("/opt/bin/codex"),
                &setup.invocation(sandboxed, extra),
            )
            .unwrap()
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    fn head(setup: &Setup) -> Vec<String> {
        strings(&[
            "/opt/bin/codex",
            "exec",
            "--json",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
            "-C",
            setup.work().to_str().unwrap(),
        ])
    }

    fn sandbox_permissions(setup: &Setup) -> Vec<String> {
        let real = std::fs::canonicalize(setup.tempdir()).unwrap();
        strings(&[
            "-c",
            "default_permissions=agentrun",
            "-c",
            &format!(
                "permissions.agentrun.filesystem={{\":root\"=\"read\", \":workspace_roots\"={{\".\"=\"write\"}}, \"{}\"=\"write\"}}",
                real.display()
            ),
        ])
    }

    fn settings(web_search: &str) -> Vec<String> {
        let mut args = strings(&[
            "-c",
            "approval_policy=\"never\"",
            "-c",
            &format!("web_search=\"{web_search}\""),
            "-c",
            "project_doc_max_bytes=0",
            "-c",
            "skills.include_instructions=false",
            "-c",
            "skills.bundled.enabled=false",
            "-c",
            "allow_login_shell=false",
        ]);
        for feature in DISABLED_FEATURES {
            args.push("--disable".to_string());
            args.push(feature.to_string());
        }
        args
    }

    fn expected(
        setup: &Setup,
        permissions: &[String],
        web_search: &str,
        rest: &[&str],
    ) -> Vec<String> {
        let mut argv = head(setup);
        argv.extend(permissions.iter().cloned());
        argv.extend(settings(web_search));
        argv.extend(strings(rest));
        argv.push("-".to_string());
        argv
    }

    fn full_access() -> Vec<String> {
        strings(&["-c", "default_permissions=\":danger-full-access\""])
    }

    #[test]
    fn sandboxed_argv_with_network_none() {
        let setup = Setup::new();
        let launch = launch(&setup, true, &[]);
        let mut permissions = sandbox_permissions(&setup);
        permissions.extend(strings(&[
            "-c",
            "permissions.agentrun.network.enabled=false",
        ]));
        assert_eq!(launch.argv, expected(&setup, &permissions, "disabled", &[]));
        assert_eq!(launch.stdin, b"hi");
        assert_eq!(
            launch.env,
            vec![(
                OsString::from("CODEX_HOME"),
                home_path(&setup.tempdir()).into_os_string()
            )]
        );
    }

    #[test]
    fn sandboxed_argv_with_network_full() {
        let setup = Setup::new();
        let launch = launch(&setup, true, &["--network", "full"]);
        let mut permissions = sandbox_permissions(&setup);
        permissions.extend(strings(&[
            "-c",
            "permissions.agentrun.network.enabled=true",
        ]));
        assert_eq!(launch.argv, expected(&setup, &permissions, "live", &[]));
        assert_eq!(launch.env.len(), 1);
    }

    #[test]
    fn sandboxed_argv_with_network_custom_dedupes_hosts_and_keeps_wildcards() {
        let setup = Setup::new();
        let launch = launch(
            &setup,
            true,
            &[
                "--network",
                "custom",
                "--allow-host",
                "api.github.com:443",
                "--allow-host",
                "*.example.com",
                "--allow-host",
                "api.github.com",
                "--allow-host",
                "pypi.org:8443",
            ],
        );
        let mut permissions = sandbox_permissions(&setup);
        permissions.extend(strings(&[
            "--enable",
            "network_proxy",
            "-c",
            "permissions.agentrun.network={enabled=true, enable_socks5=false, enable_socks5_udp=false, allow_upstream_proxy=true, domains={\"api.github.com\"=\"allow\", \"*.example.com\"=\"allow\", \"pypi.org\"=\"allow\"}}",
        ]));
        assert_eq!(launch.argv, expected(&setup, &permissions, "disabled", &[]));
        let mut env = vec![(
            OsString::from("CODEX_HOME"),
            home_path(&setup.tempdir()).into_os_string(),
        )];
        env.extend(proxy_environment("4321"));
        assert_eq!(launch.env, env);
    }

    #[test]
    fn unsandboxed_argv_uses_full_access_for_every_network_mode() {
        let setup = Setup::new();
        for (mode, web_search) in [("none", "disabled"), ("full", "live")] {
            let launch = launch(&setup, false, &["--network", mode]);
            assert_eq!(
                launch.argv,
                expected(&setup, &full_access(), web_search, &[])
            );
        }
        let launch = launch(
            &setup,
            false,
            &["--network", "custom", "--allow-host", "api.github.com"],
        );
        assert_eq!(
            launch.argv,
            expected(&setup, &full_access(), "disabled", &[])
        );
        assert_eq!(launch.env.len(), 1);
    }

    #[test]
    fn options_and_caller_arguments_come_before_the_stdin_marker() {
        let setup = Setup::new();
        let launch = launch(
            &setup,
            false,
            &[
                "--no-subagents",
                "--model",
                "gpt-5.3-codex",
                "--effort",
                "hi\"gh",
                "--",
                "-c",
                "x=1",
            ],
        );
        assert_eq!(
            launch.argv,
            expected(
                &setup,
                &full_access(),
                "disabled",
                &[
                    "-c",
                    "agents.enabled=false",
                    "--disable",
                    "multi_agent",
                    "--disable",
                    "multi_agent_v2",
                    "-m",
                    "gpt-5.3-codex",
                    "-c",
                    "model_reasoning_effort=\"hi\\\"gh\"",
                    "-c",
                    "x=1",
                ],
            )
        );
    }

    fn with_images(setup: &Setup, extra: &[&str]) -> Result<Launch, String> {
        let _ = std::fs::remove_dir_all(home_path(&setup.tempdir()));
        let mut invocation = setup.invocation(false, extra);
        invocation.images = vec![
            Image {
                given: "a.png".to_string(),
                kind: ImageKind::Png,
                bytes: b"png".to_vec(),
            },
            Image {
                given: "b.jpg".to_string(),
                kind: ImageKind::Jpeg,
                bytes: b"jpeg".to_vec(),
            },
        ];
        Codex::new().launch(Path::new("/opt/bin/codex"), &invocation)
    }

    #[test]
    fn images_are_copied_and_passed_right_after_the_working_directory() {
        let setup = Setup::new();
        let launch = with_images(&setup, &[]).unwrap();
        let first = setup.tempdir().join("image-1.png");
        let second = setup.tempdir().join("image-2.jpg");
        let head = head(&setup);
        assert_eq!(launch.argv[..head.len()], head);
        assert_eq!(
            launch.argv[head.len()..head.len() + 5],
            [
                "--image".to_string(),
                first.to_string_lossy().into_owned(),
                "--image".to_string(),
                second.to_string_lossy().into_owned(),
                "-c".to_string(),
            ]
        );
        assert_eq!(launch.argv.last().unwrap(), "-");
        assert_eq!(std::fs::read(&first).unwrap(), b"png");
        assert_eq!(std::fs::read(&second).unwrap(), b"jpeg");
    }

    #[test]
    fn dry_run_names_the_image_copies_without_writing_them() {
        let setup = Setup::new();
        let launch = with_images(&setup, &["--dry-run"]).unwrap();
        assert!(launch.argv.contains(&"<tempdir>/image-1.png".to_string()));
        assert!(!setup.tempdir().join("image-1.png").exists());
    }

    #[test]
    fn image_copy_path_with_a_comma_is_a_launch_error() {
        let setup = Setup::new();
        let mut invocation = setup.invocation(false, &["--dry-run"]);
        invocation.tempdir = PathBuf::from("/tmp/a,b");
        invocation.images = vec![Image {
            given: "a.png".to_string(),
            kind: ImageKind::Png,
            bytes: b"png".to_vec(),
        }];
        assert_eq!(
            Codex::new()
                .launch(Path::new("/opt/bin/codex"), &invocation)
                .unwrap_err(),
            "cannot give images to codex: the path /tmp/a,b/image-1.png contains ',', which codex reads as a list separator"
        );
    }

    #[test]
    fn dry_run_writes_the_placeholders() {
        let setup = Setup::new();
        let launch = launch(&setup, true, &["--dry-run"]);
        assert!(launch.argv.contains(
            &"permissions.agentrun.filesystem={\":root\"=\"read\", \":workspace_roots\"={\".\"=\"write\"}, \"<tempdir>\"=\"write\"}".to_string()
        ));
        assert!(
            !launch
                .argv
                .iter()
                .any(|arg| arg.contains("<tempdir>-codex"))
        );
        assert_eq!(launch.env[0].1, OsString::from("<tempdir>-codex"));
    }

    #[test]
    fn launch_creates_the_private_home_with_the_login_link() {
        let setup = Setup::new();
        let home = home_path(&setup.tempdir());
        let launch = launch(&setup, true, &[]);
        assert_eq!(
            launch.private_dirs,
            vec![PrivateDir {
                label: "codex home",
                path: home.clone(),
            }]
        );
        assert_eq!(
            launch.env[0],
            (OsString::from("CODEX_HOME"), home.clone().into_os_string())
        );
        assert_eq!(
            std::fs::symlink_metadata(&home)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let entries: Vec<_> = std::fs::read_dir(&home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, [OsString::from("auth.json")]);
        assert_eq!(
            std::fs::read_link(home.join("auth.json")).unwrap(),
            setup.login()
        );
    }

    #[test]
    fn dry_run_launch_creates_no_private_home() {
        let setup = Setup::new();
        let launch = launch(&setup, true, &["--dry-run"]);
        assert!(launch.private_dirs.is_empty());
        assert_eq!(launch.env[0].1, OsString::from("<tempdir>-codex"));
        assert!(!Path::new("<tempdir>-codex").exists());
        assert!(!home_path(&setup.tempdir()).exists());
    }

    #[test]
    fn missing_login_file_fails_the_launch_without_a_private_home() {
        let setup = Setup::new();
        std::fs::remove_file(setup.login()).unwrap();
        for extra in [&[][..], &["--dry-run"]] {
            let detail = Codex::new()
                .launch(Path::new("/opt/bin/codex"), &setup.invocation(true, extra))
                .unwrap_err();
            assert_eq!(
                detail,
                format!(
                    "codex login file {} not found. Run codex login, or pass its content in AGENTRUN_CODEX_AUTH",
                    setup.login().display()
                )
            );
        }
        assert!(!home_path(&setup.tempdir()).exists());
    }

    #[test]
    fn unresolvable_tempdir_fails_the_launch_without_a_private_home() {
        let setup = Setup::new();
        std::fs::remove_dir(setup.tempdir()).unwrap();
        let detail = Codex::new()
            .launch(Path::new("/opt/bin/codex"), &setup.invocation(true, &[]))
            .unwrap_err();
        assert!(
            detail.starts_with(&format!(
                "cannot resolve the session temporary directory {}: ",
                setup.tempdir().display()
            )),
            "{detail}"
        );
        assert!(!home_path(&setup.tempdir()).exists());
    }

    #[test]
    fn sandbox_command_wraps_the_permissions_with_the_profile_and_cwd() {
        let tempdir = Path::new("/tmp/session");
        let cwd = Path::new("/work/repo");
        let allow_hosts =
            network::check_usage(NetworkMode::Custom, &["example.com".to_string()]).unwrap();
        for (mode, rules) in [
            (NetworkMode::None, &[][..]),
            (NetworkMode::Full, &[][..]),
            (NetworkMode::Custom, &allow_hosts[..]),
        ] {
            let command = sandbox_command(tempdir, &[], cwd, mode, rules);
            let permissions = super::sandbox_permissions(tempdir, &[], mode, rules);
            assert_eq!(command[..3], strings(&["sandbox", "-P", "agentrun"]));
            assert_eq!(command[3..command.len() - 3], permissions);
            assert_eq!(
                command[command.len() - 3..],
                strings(&["-C", "/work/repo", "--"])
            );
            assert_eq!(
                permissions
                    .windows(2)
                    .any(|pair| pair == ["--enable", "network_proxy"]),
                mode == NetworkMode::Custom
            );
        }
    }

    #[test]
    fn sandboxed_launch_denies_reading_the_existing_denied_paths() {
        let setup = Setup::new();
        let secrets = setup.root.path().join("secrets");
        std::fs::create_dir(&secrets).unwrap();
        std::fs::write(setup.root.path().join("netrc"), "x").unwrap();
        std::os::unix::fs::symlink(
            setup.root.path().join("netrc"),
            setup.root.path().join("netrc-link"),
        )
        .unwrap();
        let invocation = Invocation {
            denied_reads: vec![
                secrets.clone(),
                setup.root.path().join("netrc-link"),
                setup.root.path().join("missing"),
            ],
            ..setup.invocation(true, &[])
        };
        let launch = Codex::new()
            .launch(Path::new("/opt/bin/codex"), &invocation)
            .unwrap();
        let real = std::fs::canonicalize(setup.root.path()).unwrap();
        let filesystem = launch
            .argv
            .iter()
            .find(|arg| arg.starts_with("permissions.agentrun.filesystem="))
            .unwrap();
        assert!(
            filesystem.ends_with(&format!(
                "=\"write\", \"{0}/secrets\"=\"deny\", \"{0}/netrc\"=\"deny\"}}",
                real.display()
            )),
            "{filesystem}"
        );
        let unsandboxed = Invocation {
            denied_reads: vec![secrets],
            ..setup.invocation(false, &[])
        };
        let _ = std::fs::remove_dir_all(home_path(&setup.tempdir()));
        let launch = Codex::new()
            .launch(Path::new("/opt/bin/codex"), &unsandboxed)
            .unwrap();
        assert!(!launch.argv.iter().any(|arg| arg.contains("deny")));
    }

    #[test]
    fn denying_the_login_file_or_its_directory_also_denies_the_private_home_link() {
        let setup = Setup::new();
        let login = setup.login();
        for denied in [login.clone(), login.parent().unwrap().to_path_buf()] {
            let invocation = Invocation {
                denied_reads: vec![denied.clone()],
                ..setup.invocation(true, &[])
            };
            let launch = Codex::new()
                .launch(Path::new("/opt/bin/codex"), &invocation)
                .unwrap();
            let denied = std::fs::canonicalize(&denied).unwrap();
            let link = home_path(&std::fs::canonicalize(setup.tempdir()).unwrap()).join(LOGIN_FILE);
            let filesystem = launch
                .argv
                .iter()
                .find(|arg| arg.starts_with("permissions.agentrun.filesystem="))
                .unwrap();
            assert!(
                filesystem.ends_with(&format!(
                    "=\"write\", \"{}\"=\"deny\", \"{}\"=\"deny\"}}",
                    denied.display(),
                    link.display()
                )),
                "{filesystem}"
            );
            assert_eq!(std::fs::read_link(&link).unwrap(), login);
            std::fs::remove_dir_all(link.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn check_args_rejects_denied_paths_with_glob_characters() {
        let setup = Setup::new();
        let root = tempfile::tempdir().unwrap();
        let bracketed = root.path().join("w[1]");
        std::fs::create_dir(&bracketed).unwrap();
        std::os::unix::fs::symlink(&bracketed, root.path().join("plain")).unwrap();
        let plain_link = root.path().join("plain").to_string_lossy().into_owned();
        let resolved = format!(
            " (resolves to {})",
            std::fs::canonicalize(&bracketed).unwrap().display()
        );
        for (path, suffix) in [
            ("/data/a*b", ""),
            ("/data/a?b", ""),
            ("/data/{a}", ""),
            (plain_link.as_str(), resolved.as_str()),
        ] {
            let invocation = setup.invocation(false, &["--deny-read", path]);
            assert_eq!(
                Codex::new().check_args(&invocation.args),
                Err(format!(
                    "--deny-read {path}{suffix}: codex cannot deny a path that contains * ? [ ] {{ }}"
                ))
            );
        }
        let invocation = setup.invocation(false, &["--deny-read", "/data/a-b"]);
        assert_eq!(Codex::new().check_args(&invocation.args), Ok(()));
    }

    #[test]
    fn check_args_rejects_the_limits() {
        let setup = Setup::new();
        let check = |extra: &[&str]| Codex::new().check_args(&setup.invocation(false, extra).args);
        let rejected = |option: &str| {
            Err(format!(
                "{option} is not supported by codex: codex reports usage only when the run ends, so agentrun cannot stop it during the run"
            ))
        };
        assert_eq!(check(&["--max-turns", "3"]), rejected("--max-turns"));
        assert_eq!(check(&["--max-tokens", "100"]), rejected("--max-tokens"));
        assert_eq!(
            check(&["--max-tokens", "100", "--max-turns", "3"]),
            rejected("--max-turns")
        );
        assert_eq!(check(&[]), Ok(()));
    }

    #[test]
    fn toml_strings_escape_quotes_backslashes_and_control_characters() {
        assert_eq!(toml_string("plain"), "\"plain\"");
        assert_eq!(toml_string("a\\b\"c"), "\"a\\\\b\\\"c\"");
        assert_eq!(toml_string("x\ny\t\x7f"), "\"x\\ny\\t\\u007F\"");
        assert_eq!(toml_string("路径/中文"), "\"路径/中文\"");
    }

    fn translate(codex: &mut Codex, line: Value) -> Vec<Record> {
        codex.translate(&line)
    }

    #[test]
    fn items_map_to_tools_and_text() {
        let mut codex = Codex::new();
        codex.cwd = PathBuf::from("/work/repo");
        assert_eq!(
            translate(
                &mut codex,
                json!({"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls -la\npwd","exit_code":0}})
            ),
            [
                Record::ToolStart {
                    id: "item_1".to_string(),
                    parent: None,
                    name: "command_execution".to_string(),
                    summary: "shell: ls -la".to_string(),
                },
                Record::ToolEnd {
                    id: "item_1".to_string(),
                    denied: false,
                    error: Some(false),
                },
            ]
        );
        assert_eq!(
            translate(
                &mut codex,
                json!({"type":"item.completed","item":{"id":"item_2","type":"file_change","changes":[{"path":"/work/repo/a.txt","kind":"add"},{"path":"/elsewhere/b.txt","kind":"update"}]}})
            )[0],
            Record::ToolStart {
                id: "item_2".to_string(),
                parent: None,
                name: "file_change".to_string(),
                summary: "patch: a.txt /elsewhere/b.txt".to_string(),
            }
        );
        assert_eq!(
            translate(
                &mut codex,
                json!({"type":"item.completed","item":{"id":"item_3","type":"mcp_tool_call","server":"fs","tool":"read"}})
            )[0],
            Record::ToolStart {
                id: "item_3".to_string(),
                parent: None,
                name: "mcp_tool_call".to_string(),
                summary: "mcp: fs.read".to_string(),
            }
        );
        assert_eq!(
            translate(
                &mut codex,
                json!({"type":"item.completed","item":{"id":"item_4","type":"agent_message","text":"done"}})
            ),
            [Record::Text {
                parent: None,
                text: "done".to_string(),
            }]
        );
        for other in [
            json!({"type":"item.started","item":{"id":"item_5","type":"agent_message","text":""}}),
            json!({"type":"item.started","item":{"id":"item_8","type":"collab_tool_call","tool":"wait"}}),
            json!({"type":"item.completed","item":{"id":"item_6","type":"collab_tool_call","tool":"wait"}}),
            json!({"type":"item.completed","item":{"id":"item_7","type":"reasoning","text":"hm"}}),
            json!({"type":"thread.started","thread_id":"t"}),
            json!({"type":"turn.started"}),
        ] {
            assert!(translate(&mut codex, other.clone()).is_empty(), "{other}");
        }
    }

    #[test]
    fn started_item_gives_tool_start_and_its_completion_only_tool_end() {
        let mut codex = Codex::new();
        codex.cwd = PathBuf::from("/work/repo");
        assert_eq!(
            translate(
                &mut codex,
                json!({"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"latexmk -xelatex\nmore","aggregated_output":"","exit_code":null,"status":"in_progress"}})
            ),
            [Record::ToolStart {
                id: "item_1".to_string(),
                parent: None,
                name: "command_execution".to_string(),
                summary: "shell: latexmk -xelatex".to_string(),
            }]
        );
        assert!(
            translate(
                &mut codex,
                json!({"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"latexmk -xelatex","status":"in_progress"}})
            )
            .is_empty()
        );
        assert_eq!(
            translate(
                &mut codex,
                json!({"type":"item.started","item":{"id":"item_2","type":"file_change","changes":[{"path":"/work/repo/a.txt","kind":"add"}],"status":"in_progress"}})
            ),
            [Record::ToolStart {
                id: "item_2".to_string(),
                parent: None,
                name: "file_change".to_string(),
                summary: "patch: a.txt".to_string(),
            }]
        );
        assert_eq!(
            translate(
                &mut codex,
                json!({"type":"item.started","item":{"id":"item_3","type":"mcp_tool_call","server":"fs","tool":"read","status":"in_progress"}})
            ),
            [Record::ToolStart {
                id: "item_3".to_string(),
                parent: None,
                name: "mcp_tool_call".to_string(),
                summary: "mcp: fs.read".to_string(),
            }]
        );
        for (id, completed, error) in [
            (
                "item_1",
                json!({"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"latexmk -xelatex","aggregated_output":"ok","exit_code":0}}),
                Some(false),
            ),
            (
                "item_2",
                json!({"type":"item.completed","item":{"id":"item_2","type":"file_change","changes":[{"path":"/work/repo/a.txt","kind":"add"}]}}),
                None,
            ),
            (
                "item_3",
                json!({"type":"item.completed","item":{"id":"item_3","type":"mcp_tool_call","server":"fs","tool":"read"}}),
                None,
            ),
        ] {
            assert_eq!(
                translate(&mut codex, completed),
                [Record::ToolEnd {
                    id: id.to_string(),
                    denied: false,
                    error,
                }]
            );
        }
        assert_eq!(
            translate(
                &mut codex,
                json!({"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls"}})
            )
            .len(),
            2
        );
    }

    #[test]
    fn item_error_comes_from_exit_code_and_status() {
        let cases = [
            (
                json!({"type":"command_execution","exit_code":0,"status":"completed"}),
                Some(false),
            ),
            (
                json!({"type":"command_execution","exit_code":56,"status":"failed"}),
                Some(true),
            ),
            (
                json!({"type":"command_execution","exit_code":1,"status":"completed"}),
                Some(true),
            ),
            (
                json!({"type":"command_execution","exit_code":null,"status":"failed"}),
                Some(true),
            ),
            (
                json!({"type":"command_execution","exit_code":null,"status":"declined"}),
                None,
            ),
            (
                json!({"type":"command_execution","exit_code":0,"status":"declined"}),
                None,
            ),
            (
                json!({"type":"command_execution","exit_code":0}),
                Some(false),
            ),
            (
                json!({"type":"file_change","status":"completed"}),
                Some(false),
            ),
            (json!({"type":"file_change","status":"failed"}), Some(true)),
            (
                json!({"type":"mcp_tool_call","status":"failed"}),
                Some(true),
            ),
            (json!({"type":"mcp_tool_call"}), None),
        ];
        for (item, error) in cases {
            assert_eq!(item_error(&item), error, "{item}");
        }
    }

    #[test]
    fn bwrap_output_after_a_started_item_is_still_a_sandbox_failure() {
        let mut codex = Codex::new();
        codex.sandboxed = true;
        translate(
            &mut codex,
            json!({"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":""}}),
        );
        assert_eq!(
            translate(
                &mut codex,
                bwrap_output("item_1", "bwrap: setting up uid map: Permission denied")
            ),
            [Record::ToolEnd {
                id: "item_1".to_string(),
                denied: false,
                error: Some(true),
            }]
        );
        assert_eq!(
            codex.failure(Some(0)),
            Some(Failure::Message(
                "sandbox failed to start: setting up uid map: Permission denied".to_string()
            ))
        );
    }

    #[test]
    fn turn_completed_maps_usage_with_the_model_option() {
        let mut codex = Codex::new();
        codex.model = Some("gpt-5.3-codex".to_string());
        assert_eq!(
            translate(
                &mut codex,
                json!({"type":"turn.completed","usage":{"input_tokens":18527,"cached_input_tokens":8960,"cache_write_input_tokens":3,"output_tokens":109,"reasoning_output_tokens":5}})
            ),
            [Record::Usage {
                parent: None,
                model: Some("gpt-5.3-codex".to_string()),
                counts: TokenCounts {
                    input_tokens: Some(9567),
                    output_tokens: Some(109),
                    cache_read_tokens: Some(8960),
                    cache_write_tokens: Some(3),
                    cost_usd: None,
                },
            }]
        );
        let mut codex = Codex::new();
        assert_eq!(
            translate(
                &mut codex,
                json!({"type":"turn.completed","usage":{"input_tokens":5,"cached_input_tokens":9}})
            ),
            [Record::Usage {
                parent: None,
                model: None,
                counts: TokenCounts {
                    input_tokens: Some(0),
                    output_tokens: Some(0),
                    cache_read_tokens: Some(9),
                    cache_write_tokens: Some(0),
                    cost_usd: None,
                },
            }]
        );
    }

    #[test]
    fn failure_rules_apply_in_order() {
        let long = "e".repeat(600);
        let mut codex = Codex::new();
        translate(&mut codex, json!({"type":"error","message":"first"}));
        translate(&mut codex, json!({"type":"error","message":"retry"}));
        translate(&mut codex, json!({"type":"turn.completed","usage":{}}));
        translate(
            &mut codex,
            json!({"type":"turn.failed","error":{"message":"old"}}),
        );
        translate(
            &mut codex,
            json!({"type":"turn.failed","error":{"message":long.clone()}}),
        );
        assert_eq!(
            codex.failure(Some(0)),
            Some(Failure::Message("e".repeat(500)))
        );

        let mut codex = Codex::new();
        translate(&mut codex, json!({"type":"error","message":"first"}));
        translate(&mut codex, json!({"type":"error","message":long}));
        assert_eq!(
            codex.failure(Some(0)),
            Some(Failure::Message("e".repeat(500)))
        );

        let mut codex = Codex::new();
        translate(&mut codex, json!({"type":"error","message":"retry"}));
        translate(&mut codex, json!({"type":"turn.completed","usage":{}}));
        assert_eq!(codex.failure(Some(0)), None);
        assert_eq!(codex.failure(Some(1)), Some(Failure::Unexplained));

        let codex = Codex::new();
        assert_eq!(codex.failure(Some(2)), Some(Failure::Unexplained));
        assert_eq!(
            codex.failure(Some(0)),
            Some(Failure::Message(NO_TURN_DETAIL.to_string()))
        );
        assert_eq!(codex.failure(None), Some(Failure::Unexplained));

        let mut codex = Codex::new();
        translate(&mut codex, json!({"type":"turn.failed","error":{}}));
        assert_eq!(codex.failure(Some(0)), Some(Failure::Unexplained));
    }

    fn bwrap_output(id: &str, output: &str) -> Value {
        json!({"type":"item.completed","item":{"id":id,"type":"command_execution","command":"ls","aggregated_output":output,"exit_code":1}})
    }

    #[test]
    fn bwrap_output_in_a_sandboxed_run_is_a_sandbox_failure() {
        let mut codex = Codex::new();
        codex.sandboxed = true;
        translate(
            &mut codex,
            bwrap_output("item_1", "plain output\nbwrap: late"),
        );
        assert_eq!(
            codex
                .translate(&bwrap_output(
                    "item_2",
                    "bwrap: setting up uid map: Permission denied\nmore"
                ))
                .len(),
            2
        );
        translate(&mut codex, bwrap_output("item_3", "bwrap: second failure"));
        translate(
            &mut codex,
            json!({"type":"turn.failed","error":{"message":"later"}}),
        );
        assert_eq!(
            codex.failure(Some(0)),
            Some(Failure::Message(
                "sandbox failed to start: setting up uid map: Permission denied".to_string()
            ))
        );

        let mut codex = Codex::new();
        codex.sandboxed = true;
        translate(&mut codex, bwrap_output("item_1", "bwrap:no space"));
        translate(&mut codex, json!({"type":"turn.completed","usage":{}}));
        assert_eq!(codex.failure(Some(0)), None);

        let mut codex = Codex::new();
        translate(
            &mut codex,
            bwrap_output("item_1", "bwrap: setting up uid map: Permission denied"),
        );
        translate(&mut codex, json!({"type":"turn.completed","usage":{}}));
        assert_eq!(codex.failure(Some(0)), None);
    }

    #[test]
    fn login_file_follows_the_session_environment() {
        let env: Vec<(OsString, OsString)> = vec![
            (OsString::from("HOME"), OsString::from("/home/u")),
            (OsString::from("CODEX_HOME"), OsString::from("state")),
        ];
        let session = Session::assemble(Runtime::Codex, &env, &[], &[], &[]);
        assert_eq!(
            login_file(&session, Path::new("/work")),
            PathBuf::from("/work/state/auth.json")
        );
        let session = Session::assemble(Runtime::Codex, &env[..1], &[], &[], &[]);
        assert_eq!(
            login_file(&session, Path::new("/work")),
            PathBuf::from("/home/u/.codex/auth.json")
        );
        let session = Session::assemble(Runtime::Codex, &[], &[], &[], &[]);
        assert_eq!(
            login_file(&session, Path::new("/work")),
            PathBuf::from("$HOME/.codex/auth.json")
        );
    }

    #[test]
    fn user_home_prefers_codex_home_then_home() {
        let env: Vec<(OsString, OsString)> = vec![
            (OsString::from("HOME"), OsString::from("/home/u")),
            (OsString::from("CODEX_HOME"), OsString::from("state")),
        ];
        let session = Session::assemble(Runtime::Codex, &env, &[], &[], &[]);
        assert_eq!(
            user_home(&session, Path::new("/work")),
            Some(PathBuf::from("/work/state"))
        );
        let session = Session::assemble(Runtime::Codex, &env[..1], &[], &[], &[]);
        assert_eq!(
            user_home(&session, Path::new("/work")),
            Some(PathBuf::from("/home/u/.codex"))
        );
        let session = Session::assemble(Runtime::Codex, &[], &[], &[], &[]);
        assert_eq!(user_home(&session, Path::new("/work")), None);
    }

    #[test]
    fn home_is_created_next_to_the_tempdir_with_the_login_link() {
        let setup = Setup::new();
        let home = home_path(&setup.tempdir());
        assert_eq!(
            home.file_name().unwrap().to_str().unwrap(),
            "agentrun-session-codex"
        );
        let login = setup.root.path().join("elsewhere/auth.json");
        create_home(&home, &login).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&home)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(std::fs::read_link(home.join("auth.json")).unwrap(), login);
        let error = create_home(&home, &login).unwrap_err();
        assert!(
            error.starts_with(&format!(
                "cannot create the codex home directory {}: ",
                home.display()
            )),
            "{error}"
        );
    }

    #[test]
    fn login_check_passes_when_the_file_exists_or_its_content_is_given() {
        let setup = Setup::new();
        let login = setup.root.path().join("auth.json");
        let session = Session::assemble(Runtime::Codex, &[], &[], &[], &[]);
        assert_eq!(
            check_login(&session, &login).unwrap_err(),
            format!(
                "codex login file {} not found. Run codex login, or pass its content in AGENTRUN_CODEX_AUTH",
                login.display()
            )
        );
        let empty = vec![(OsString::from("AGENTRUN_CODEX_AUTH"), OsString::new())];
        let session = Session::assemble(Runtime::Codex, &empty, &[], &[], &[]);
        assert!(check_login(&session, &login).is_err());
        let given = vec![(OsString::from("AGENTRUN_CODEX_AUTH"), OsString::from("{}"))];
        let session = Session::assemble(Runtime::Codex, &given, &[], &[], &[]);
        assert_eq!(check_login(&session, &login), Ok(()));
        std::fs::write(&login, "{}").unwrap();
        let session = Session::assemble(Runtime::Codex, &[], &[], &[], &[]);
        assert_eq!(check_login(&session, &login), Ok(()));
    }

    const SAVED_ID: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";

    fn resumed(setup: &Setup, extra: &[&str]) -> (Codex, Result<Launch, String>) {
        let saved = setup.root.path().join("saved/codex");
        std::fs::create_dir_all(&saved).unwrap();
        let file = saved.join(format!("rollout-2026-10-19T03-04-46-{SAVED_ID}.jsonl"));
        std::fs::write(&file, "{\"old\":1}\n").unwrap();
        let mut args = vec!["--session-dir", "/saved", "--resume", SAVED_ID];
        args.extend(extra);
        let mut invocation = setup.invocation(false, &args);
        invocation.resume = Some(crate::transcript::Saved {
            id: SAVED_ID.to_string(),
            file,
        });
        let _ = std::fs::remove_dir_all(home_path(&setup.tempdir()));
        let mut codex = Codex::new();
        let launch = codex.launch(Path::new("/opt/bin/codex"), &invocation);
        (codex, launch)
    }

    #[test]
    fn resume_copies_the_saved_rollout_into_the_private_home() {
        let setup = Setup::new();
        let (codex, launch) = resumed(&setup, &["--", "-c", "x=1"]);
        let launch = launch.unwrap();
        let tail: Vec<&str> = launch.argv[launch.argv.len() - 5..]
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(tail, ["-c", "x=1", "resume", SAVED_ID, "-"]);
        let copied = home_path(&setup.tempdir()).join(format!(
            "sessions/rollout-2026-10-19T03-04-46-{SAVED_ID}.jsonl"
        ));
        assert_eq!(std::fs::read_to_string(&copied).unwrap(), "{\"old\":1}\n");
        assert_eq!(
            codex.transcript(),
            Some(Transcript {
                id: SAVED_ID.to_string(),
                entries: vec![copied],
                emptied_dir: None,
            })
        );
    }

    #[test]
    fn dry_run_resume_copies_nothing() {
        let setup = Setup::new();
        let (_, launch) = resumed(&setup, &["--dry-run"]);
        let launch = launch.unwrap();
        assert!(launch.private_dirs.is_empty());
        assert_eq!(
            launch.argv[launch.argv.len() - 3..],
            ["resume", SAVED_ID, "-"]
        );
        assert!(!home_path(&setup.tempdir()).exists());
    }

    #[test]
    fn transcript_of_a_new_session_follows_the_thread_id() {
        let setup = Setup::new();
        let mut codex = Codex::new();
        codex
            .launch(
                Path::new("/opt/bin/codex"),
                &setup.invocation(false, &["--session-dir", "/saved"]),
            )
            .unwrap();
        assert_eq!(codex.transcript(), None);
        translate(
            &mut codex,
            json!({"type":"thread.started","thread_id":SAVED_ID}),
        );
        assert_eq!(codex.transcript(), None);
        let file = home_path(&setup.tempdir()).join(format!(
            "sessions/2026/10/19/rollout-2026-10-19T03-04-46-{SAVED_ID}.jsonl"
        ));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{}\n").unwrap();
        assert_eq!(
            codex.transcript(),
            Some(Transcript {
                id: SAVED_ID.to_string(),
                entries: vec![file.clone()],
                emptied_dir: None,
            })
        );
        translate(
            &mut codex,
            json!({"type":"thread.started","thread_id":"0199-other"}),
        );
        assert_eq!(codex.transcript().unwrap().id, SAVED_ID);

        let mut unsaved = Codex::new();
        let _ = std::fs::remove_dir_all(home_path(&setup.tempdir()));
        unsaved
            .launch(Path::new("/opt/bin/codex"), &setup.invocation(false, &[]))
            .unwrap();
        translate(
            &mut unsaved,
            json!({"type":"thread.started","thread_id":SAVED_ID}),
        );
        assert_eq!(unsaved.transcript(), None);
    }
}
