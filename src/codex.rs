use std::ffi::OsString;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::adapter::{Adapter, Launch, Record};
use crate::cli::{NetworkMode, Runtime};
use crate::json::{first_line, string};
use crate::network::{HostRule, proxy_environment};
use crate::run::Invocation;
use crate::session::Session;
use crate::usage::TokenCounts;

pub const HOME_VARIABLE: &str = "CODEX_HOME";
pub const HOME_SUBDIR: &str = ".codex";
pub const HOME_SUFFIX: &str = "-codex";
pub const LOGIN_FILE: &str = "auth.json";
pub const SERVICE_HOSTS: [&str; 4] = [
    "chatgpt.com",
    "ab.chatgpt.com",
    "auth.openai.com",
    "api.openai.com",
];
const FIXED_ARGS: [&str; 6] = [
    "exec",
    "--json",
    "--skip-git-repo-check",
    "--ignore-user-config",
    "--ignore-rules",
    "-C",
];
pub const PROFILE: &str = "agentrun";
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
const NO_TURN_DETAIL: &str = "codex produced no turn.completed";
const BWRAP_PREFIX: &str = "bwrap: ";
const DETAIL_MAX_CHARS: usize = 500;

pub struct Codex {
    cwd: PathBuf,
    model: Option<String>,
    sandboxed: bool,
    sandbox_failure: Option<String>,
    turn_completed: bool,
    turn_failed: Option<String>,
    last_error: Option<String>,
}

impl Default for Codex {
    fn default() -> Self {
        Self::new()
    }
}

impl Codex {
    pub fn new() -> Codex {
        Codex {
            cwd: PathBuf::new(),
            model: None,
            sandboxed: false,
            sandbox_failure: None,
            turn_completed: false,
            turn_failed: None,
            last_error: None,
        }
    }

    fn item_completed(&mut self, item: &Value) -> Vec<Record> {
        let summary = match item["type"].as_str() {
            Some("command_execution") => {
                self.note_sandbox_failure(&item["aggregated_output"]);
                format!("shell: {}", first_line(&string(&item["command"])))
            }
            Some("file_change") => format!("patch: {}", self.changed_paths(&item["changes"])),
            Some("mcp_tool_call") => {
                format!("mcp: {}.{}", string(&item["server"]), string(&item["tool"]))
            }
            Some("agent_message") => {
                return vec![Record::Text {
                    parent: None,
                    text: string(&item["text"]),
                }];
            }
            _ => return Vec::new(),
        };
        let id = string(&item["id"]);
        vec![
            Record::ToolStart {
                id: id.clone(),
                parent: None,
                name: string(&item["type"]),
                summary,
            },
            Record::ToolEnd { id, denied: false },
        ]
    }

    fn note_sandbox_failure(&mut self, output: &Value) {
        if !self.sandboxed || self.sandbox_failure.is_some() {
            return;
        }
        if let Some(reason) = first_line(&string(output)).strip_prefix(BWRAP_PREFIX) {
            self.sandbox_failure = Some(format!("sandbox failed to start: {reason}"));
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

    fn launch(&mut self, executable: &Path, invocation: &Invocation) -> Result<Launch, String> {
        self.cwd = invocation.cwd.clone();
        self.model = invocation.args.model.clone();
        self.sandboxed = invocation.sandbox.runs();
        let home = invocation
            .codex_home
            .as_ref()
            .ok_or_else(|| "the codex home directory is not prepared".to_string())?;
        let network = invocation.args.network;
        let mut argv = vec![executable.to_string_lossy().into_owned()];
        argv.extend(FIXED_ARGS.iter().map(|arg| arg.to_string()));
        argv.push(invocation.cwd.to_string_lossy().into_owned());
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
            argv.extend(config(filesystem_setting(&tempdir)));
            argv.extend(network_settings(network, &invocation.allow_hosts));
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
        argv.push("-".to_string());
        let mut env = vec![(OsString::from(HOME_VARIABLE), home.clone().into_os_string())];
        let mut service_hosts = Vec::new();
        if let Some(proxy) = &invocation.proxy {
            env.extend(proxy_environment(&proxy.port_text()));
            service_hosts.extend(SERVICE_HOSTS.iter().map(|host| host.to_string()));
        }
        Ok(Launch {
            argv,
            stdin: invocation.prompt.clone().into_bytes(),
            env,
            signal_wrapped_child: false,
            service_hosts,
        })
    }

    fn echoes_prompt(&self) -> bool {
        false
    }

    fn translate(&mut self, line: &Value) -> Vec<Record> {
        match line["type"].as_str() {
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

    fn failure(&self, exit_code: Option<i32>, _stderr_tail: &str) -> Option<String> {
        if let Some(detail) = &self.sandbox_failure {
            return Some(detail.clone());
        }
        if let Some(message) = &self.turn_failed {
            return Some(truncate(message));
        }
        if let (false, Some(message)) = (self.turn_completed, &self.last_error) {
            return Some(truncate(message));
        }
        if exit_code != Some(0) {
            return Some(String::new());
        }
        (!self.turn_completed).then(|| NO_TURN_DETAIL.to_string())
    }
}

pub fn login_file(session: &Session, cwd: &Path) -> PathBuf {
    session
        .runtime_dir(cwd, HOME_VARIABLE, HOME_SUBDIR)
        .unwrap_or_else(|| PathBuf::from("$HOME").join(HOME_SUBDIR))
        .join(LOGIN_FILE)
}

pub fn check_login(session: &Session, login: &Path) -> Result<(), String> {
    let provided = session
        .codex_auth
        .as_ref()
        .is_some_and(|value| !value.is_empty());
    if provided || login.exists() {
        Ok(())
    } else {
        Err(format!(
            "codex login file {} not found. Run codex login, or pass its content in AGENTRUN_CODEX_AUTH",
            login.display()
        ))
    }
}

pub fn home_path(tempdir: &Path) -> PathBuf {
    let mut name = tempdir.as_os_str().to_owned();
    name.push(HOME_SUFFIX);
    PathBuf::from(name)
}

pub fn create_home(home: &Path, login: &Path) -> Result<(), String> {
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

pub fn config(value: String) -> [String; 2] {
    ["-c".to_string(), value]
}

pub fn filesystem_setting(tempdir: &Path) -> String {
    format!(
        "permissions.{PROFILE}.filesystem={{\":root\"=\"read\", \":workspace_roots\"={{\".\"=\"write\"}}, {}=\"write\"}}",
        toml_string(&tempdir.to_string_lossy())
    )
}

pub fn network_settings(network: NetworkMode, allow_hosts: &[HostRule]) -> Vec<String> {
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

pub fn toml_string(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                quoted.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

fn counts(usage: &Value) -> TokenCounts {
    let field = |name: &str| usage[name].as_u64().unwrap_or(0);
    let cached = field("cached_input_tokens");
    TokenCounts {
        input_tokens: Some(field("input_tokens").saturating_sub(cached)),
        output_tokens: Some(field("output_tokens")),
        cache_read_tokens: Some(cached),
        cache_write_tokens: Some(field("cache_write_input_tokens")),
    }
}

fn truncate(message: &str) -> String {
    message.chars().take(DETAIL_MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    use clap::Parser;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::cli::{Cli, Format, Parsed, SandboxMode};
    use crate::event::SandboxKind;
    use crate::network::{self, ProxyEndpoint};
    use crate::sandbox::Sandbox;

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
            setup
        }

        fn work(&self) -> PathBuf {
            self.root.path().join("work")
        }

        fn tempdir(&self) -> PathBuf {
            self.root.path().join("agentrun-session")
        }

        fn invocation(&self, sandboxed: bool, extra: &[&str]) -> Invocation {
            let mut args = vec!["agentrun", "codex", "--prompt", "hi"];
            args.extend(extra);
            let (runtime, args) = match Cli::try_parse_from(args).unwrap().command.into_parsed() {
                Parsed::Run(runtime, args) => (runtime, args),
                _ => unreachable!("the tests parse runtime subcommands"),
            };
            let allow_hosts = network::check_usage(args.network, &args.allow_host).unwrap();
            let proxy = (sandboxed && args.network == NetworkMode::Custom).then(|| ProxyEndpoint {
                port: Some(4321),
                socket: self.tempdir().join("proxy.sock"),
            });
            let dry_run = args.dry_run;
            Invocation {
                runtime,
                args,
                cwd: self.work(),
                prompt: "hi".to_string(),
                format: Format::Jsonl,
                sandbox: Sandbox {
                    mode: SandboxMode::On,
                    kind: if sandboxed {
                        SandboxKind::Codex
                    } else {
                        SandboxKind::None
                    },
                    reason: String::new(),
                    bwrap: None,
                    socat: None,
                    description: String::new(),
                },
                tempdir: if dry_run {
                    PathBuf::from("<tempdir>")
                } else {
                    self.tempdir()
                },
                session: Session::assemble(runtime, &[], &[], &[], &[]),
                allow_hosts,
                proxy,
                codex_home: Some(if dry_run {
                    PathBuf::from("<tempdir>-codex")
                } else {
                    home_path(&self.tempdir())
                }),
            }
        }
    }

    fn launch(setup: &Setup, sandboxed: bool, extra: &[&str]) -> Launch {
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
        assert!(!launch.signal_wrapped_child);
        assert!(launch.service_hosts.is_empty());
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
        assert!(launch.service_hosts.is_empty());
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
        assert_eq!(
            launch.service_hosts,
            [
                "chatgpt.com",
                "ab.chatgpt.com",
                "auth.openai.com",
                "api.openai.com"
            ]
        );
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
            assert!(launch.service_hosts.is_empty());
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
    fn toml_strings_escape_quotes_backslashes_and_control_characters() {
        assert_eq!(toml_string("plain"), "\"plain\"");
        assert_eq!(toml_string("a\\b\"c"), "\"a\\\\b\\\"c\"");
        assert_eq!(toml_string("x\ny\t\x7f"), "\"x\\u000Ay\\u0009\\u007F\"");
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
            json!({"type":"item.started","item":{"id":"item_5","type":"command_execution","command":"ls"}}),
            json!({"type":"item.completed","item":{"id":"item_6","type":"collab_tool_call","tool":"wait"}}),
            json!({"type":"item.completed","item":{"id":"item_7","type":"reasoning","text":"hm"}}),
            json!({"type":"thread.started","thread_id":"t"}),
            json!({"type":"turn.started"}),
        ] {
            assert!(translate(&mut codex, other.clone()).is_empty(), "{other}");
        }
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
        assert_eq!(codex.failure(Some(0), ""), Some("e".repeat(500)));

        let mut codex = Codex::new();
        translate(&mut codex, json!({"type":"error","message":"first"}));
        translate(&mut codex, json!({"type":"error","message":long}));
        assert_eq!(codex.failure(Some(0), ""), Some("e".repeat(500)));

        let mut codex = Codex::new();
        translate(&mut codex, json!({"type":"error","message":"retry"}));
        translate(&mut codex, json!({"type":"turn.completed","usage":{}}));
        assert_eq!(codex.failure(Some(0), ""), None);
        assert_eq!(codex.failure(Some(1), "stderr"), Some(String::new()));

        let codex = Codex::new();
        assert_eq!(codex.failure(Some(2), "usage"), Some(String::new()));
        assert_eq!(codex.failure(Some(0), ""), Some(NO_TURN_DETAIL.to_string()));
        assert_eq!(codex.failure(None, ""), Some(String::new()));
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
            codex.failure(Some(0), ""),
            Some("sandbox failed to start: setting up uid map: Permission denied".to_string())
        );

        let mut codex = Codex::new();
        codex.sandboxed = true;
        translate(&mut codex, bwrap_output("item_1", "bwrap:no space"));
        translate(&mut codex, json!({"type":"turn.completed","usage":{}}));
        assert_eq!(codex.failure(Some(0), ""), None);

        let mut codex = Codex::new();
        translate(
            &mut codex,
            bwrap_output("item_1", "bwrap: setting up uid map: Permission denied"),
        );
        translate(&mut codex, json!({"type":"turn.completed","usage":{}}));
        assert_eq!(codex.failure(Some(0), ""), None);
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
}
