use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{Adapter, Failure, Invocation, Launch, Version, detail_head, display_path};
use crate::cli::{NetworkMode, RunArgs, Runtime};
use crate::json::{first_line, optional_string, string};
use crate::network::{proxy_environment, url_host};
use crate::output::{Record, SandboxKind, TokenCounts};
use crate::sandbox;
use crate::session::Session;
use crate::transcript::{Transcript, copy_dir_into};

pub(crate) const MIN_VERSION: Version = Version(0, 2, 1);
const SERVICE_HOST: &str = "api.deepseek.com";
pub(crate) const KEY_VARIABLE: &str = "DEEPSEEK_API_KEY";
const BASE_URL_VARIABLE: &str = "DEEPSEEK_BASE_URL";
pub(crate) const REMOVED_PREFIX: &[u8] = b"DSH_";
const HOME_VARIABLE: &str = "DSH_HOME";
const AGENTS_VARIABLE: &str = "DSH_AGENTS_HOME";
const FIXED_ENV: [(&str, &str); 2] = [
    ("DSH_PERMISSION_MODE", "danger-full-access"),
    ("DSH_TELEMETRY_DISABLED", "1"),
];
const HOME_SUBDIR: &str = "dsh-home";
const AGENTS_SUBDIR: &str = "dsh-agents";
const PATCH_FILE: &str = "dsh-patch.yml";
const SESSIONS_SUBDIR: &str = "sessions";
const USER_HOME_SUBDIR: &str = ".dsh";
pub(crate) const ENV_FILE: &str = ".env";
const SESSION_PREFIX: &str = "session-";
const PROFILE: &str = "headless";
const STDIN_TASK: &str = "-";
const DISABLED_ENTRIES: [&str; 7] = [
    "agent-instructions",
    "skill-filesystem",
    "session-log-deepseek",
    "plugin-package-inventory-deepseek",
    "session-telemetry-otel",
    "session-title-llm",
    "tool-web",
];
const SUBAGENT_ENTRIES: [&str; 5] = [
    "tool-subagent",
    "tool-subagent-fork",
    "tool-workflow",
    "tool-subagent-control",
    "tool-subagent-list-agents",
];
const MODEL_ENTRY: &str = "agent-default-model";
const MODEL_PROVIDER: &str = "deepseek-official";
const EFFORT_ENTRY: &str = "llm-deepseek";
const COMPLETED: &str = "completed";
const ERROR_KIND: &str = "error";
const NO_IMAGES: &str = "--image is not supported by dsh: its headless mode takes text only";
const NO_TURN_END_DETAIL: &str = "dsh produced no turn_end";
const SHELL_TOOLS: [&str; 2] = ["bash", "pwsh"];
const EXIT_CODE_MARKER: &str = "\n[exit code: ";
const SIGNAL_MARKER: &str = "\n[killed by signal: ";
const PROJECT_KEY_MAX: usize = 251;

#[derive(Default)]
pub struct Dsh {
    cwd: PathBuf,
    model: Option<String>,
    home: PathBuf,
    saving: bool,
    session_id: Option<String>,
    turn_end: Option<Value>,
    error: Option<String>,
    shell_calls: HashSet<String>,
}

impl Dsh {
    pub fn new() -> Dsh {
        Dsh::default()
    }

    fn tool_summary(&self, name: &str, input: &Value) -> String {
        let value = match name {
            "bash" => input["command"]
                .as_str()
                .map(|command| first_line(command).to_string()),
            "read" | "write" | "edit" | "read_image" => input["file_path"]
                .as_str()
                .map(|path| display_path(Path::new(path), &self.cwd)),
            "glob" | "grep" => input["pattern"].as_str().map(str::to_string),
            _ => None,
        };
        match value {
            Some(value) => format!("{name}: {value}"),
            None => name.to_string(),
        }
    }

    fn status(&mut self, line: &Value) -> Vec<Record> {
        match line["phase"].as_str() {
            Some("step_start") => vec![Record::CallStart { parent: None }],
            Some("step_end") if line["usage"].is_object() => vec![Record::Usage {
                parent: None,
                model: self.model.clone(),
                counts: counts(&line["usage"]),
            }],
            Some("turn_end") => {
                self.turn_end = Some(line["reason"].clone());
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

impl Adapter for Dsh {
    fn runtime(&self) -> Runtime {
        Runtime::Dsh
    }

    fn check_args(&self, args: &RunArgs) -> Result<(), String> {
        if !args.image.is_empty() {
            return Err(NO_IMAGES.to_string());
        }
        Ok(())
    }

    fn launch(&mut self, executable: &Path, invocation: &Invocation) -> Result<Launch, String> {
        check_key(&invocation.session)?;
        let dry_run = invocation.args.dry_run;
        self.cwd =
            std::fs::canonicalize(&invocation.cwd).unwrap_or_else(|_| invocation.cwd.clone());
        self.model = invocation.args.model.clone();
        self.saving = invocation.args.session_dir.is_some();
        let home = invocation.tempdir.join(HOME_SUBDIR);
        let agents = invocation.tempdir.join(AGENTS_SUBDIR);
        let patch = invocation.tempdir.join(PATCH_FILE);
        self.home = home.clone();
        let mut argv = vec![
            executable.to_string_lossy().into_owned(),
            PROFILE.to_string(),
            "--patch".to_string(),
            patch.to_string_lossy().into_owned(),
            "--json".to_string(),
        ];
        if let Some(saved) = &invocation.resume {
            let id = format!("{SESSION_PREFIX}{}", saved.id);
            argv.push("--session-id".to_string());
            argv.push(id.clone());
            self.session_id = Some(id);
        }
        argv.extend(invocation.args.runtime_args.iter().cloned());
        argv.push(STDIN_TASK.to_string());
        if !dry_run {
            create_private_dir(&home)?;
            create_private_dir(&agents)?;
            write_patch(&patch, &invocation.args)?;
            if let Some(saved) = &invocation.resume {
                let project = home
                    .join(SESSIONS_SUBDIR)
                    .join(project_key(&self.cwd.to_string_lossy()));
                copy_dir_into(&saved.file, &project)?;
            }
        }
        let mut env = vec![
            (OsString::from(HOME_VARIABLE), home.into_os_string()),
            (OsString::from(AGENTS_VARIABLE), agents.into_os_string()),
        ];
        env.extend(
            FIXED_ENV
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        );
        let mut service_hosts = Vec::new();
        if let Some(proxy) = &invocation.proxy {
            match service_host(&invocation.session) {
                Some(host) => service_hosts.push(host),
                None if invocation.args.network == NetworkMode::None => {
                    return Err(format!(
                        "cannot tell which host dsh's model service uses from {BASE_URL_VARIABLE}. Use --network custom --allow-host <host of the model service>"
                    ));
                }
                None => {}
            }
            env.extend(proxy_environment(&proxy.port_text()));
        }
        let wrapped = sandbox::wrap(
            &invocation.sandbox,
            &invocation.cwd,
            &invocation.tempdir,
            None,
            &invocation.denied_reads,
            invocation.proxy.as_ref(),
            dry_run,
        )?;
        Ok(Launch {
            argv: wrapped.argv(&argv),
            stdin: invocation.prompt.clone().into_bytes(),
            env,
            signal_wrapped_child: wrapped.kind == SandboxKind::Bubblewrap,
            service_hosts,
            wrapped: wrapped.kind,
            private_dirs: Vec::new(),
        })
    }

    fn echoes_prompt(&self) -> bool {
        false
    }

    fn translate(&mut self, line: &Value) -> Vec<Record> {
        match line["type"].as_str() {
            Some("session") => {
                if self.session_id.is_none() {
                    self.session_id = optional_string(&line["sessionId"]);
                }
                Vec::new()
            }
            Some("status") => self.status(line),
            Some("text") => {
                let text = string(&line["text"]);
                if text.is_empty() {
                    return Vec::new();
                }
                vec![Record::Text { parent: None, text }]
            }
            Some("tool_call") => {
                let id = string(&line["callId"]);
                let name = string(&line["tool"]);
                let summary = self.tool_summary(&name, &line["input"]);
                if SHELL_TOOLS.contains(&name.as_str()) {
                    self.shell_calls.insert(id.clone());
                }
                vec![Record::ToolStart {
                    id,
                    parent: None,
                    name,
                    summary,
                }]
            }
            Some("tool_result") => {
                let id = string(&line["callId"]);
                let shell = self.shell_calls.remove(&id);
                let error = match line["status"].as_str() {
                    Some("error") => Some(true),
                    Some(COMPLETED) if shell => line["result"].as_str().map(command_failed),
                    Some(COMPLETED) => Some(false),
                    _ => None,
                };
                vec![Record::ToolEnd {
                    id,
                    denied: false,
                    error,
                }]
            }
            Some("final") => {
                let text = string(&line["text"]);
                if text.is_empty() {
                    return Vec::new();
                }
                vec![Record::Result { text }]
            }
            Some("error") => {
                self.error = Some(string(&line["message"]));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn after_exit(&mut self) -> Vec<Record> {
        Vec::new()
    }

    fn failure(&self, exit_code: Option<i32>) -> Option<Failure> {
        if let Some(message) = &self.error {
            return Some(Failure::from_detail(detail_head(message)));
        }
        if let Some(reason) = &self.turn_end {
            match reason["kind"].as_str() {
                Some(COMPLETED) => {}
                Some(ERROR_KIND) => {
                    return Some(Failure::from_detail(detail_head(&error_text(
                        &reason["error"],
                    ))));
                }
                kind => {
                    return Some(Failure::Message(format!(
                        "turn_end reason: {}",
                        kind.unwrap_or_default()
                    )));
                }
            }
        }
        if exit_code != Some(0) {
            return Some(Failure::Unexplained);
        }
        self.turn_end
            .is_none()
            .then(|| Failure::Message(NO_TURN_END_DETAIL.to_string()))
    }

    fn transcript(&self) -> Option<Transcript> {
        if !self.saving {
            return None;
        }
        let name = self.session_id.clone()?;
        let id = name.strip_prefix(SESSION_PREFIX)?.to_string();
        let dir = std::fs::read_dir(self.home.join(SESSIONS_SUBDIR))
            .ok()?
            .flatten()
            .filter(|project| project.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|project| project.path().join(&name))
            .find(|dir| is_real_dir(dir))?;
        Some(Transcript {
            id,
            entries: vec![dir],
            emptied_dir: None,
        })
    }
}

pub(crate) fn user_home(session: &Session, cwd: &Path) -> Option<PathBuf> {
    session
        .removed_values
        .get(OsStr::new(HOME_VARIABLE))
        .filter(|home| !home.is_empty())
        .map(|home| cwd.join(home))
        .or_else(|| session.runtime_dir(cwd, HOME_VARIABLE, USER_HOME_SUBDIR))
}

pub(crate) fn history_paths(session: &Session, cwd: &Path) -> Vec<PathBuf> {
    user_home(session, cwd)
        .map(|home| home.join(SESSIONS_SUBDIR))
        .into_iter()
        .collect()
}

fn command_failed(result: &str) -> bool {
    let marker_value = |marker: &str| {
        result
            .rsplit_once(marker)
            .and_then(|(_, rest)| rest.strip_suffix(']'))
            .filter(|value| !value.is_empty() && !value.contains([']', '\n']))
    };
    marker_value(SIGNAL_MARKER).is_some()
        || marker_value(EXIT_CODE_MARKER)
            .and_then(|code| code.parse::<u32>().ok())
            .is_some_and(|code| code != 0)
}

pub(crate) fn saved_session(store: &Path, id: &str) -> Option<PathBuf> {
    let dir = store.join(format!("{SESSION_PREFIX}{id}"));
    is_real_dir(&dir).then_some(dir)
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn check_key(session: &Session) -> Result<(), String> {
    let given = session
        .env
        .get(OsStr::new(KEY_VARIABLE))
        .is_some_and(|value| !value.is_empty());
    if given {
        Ok(())
    } else {
        Err(format!(
            "{KEY_VARIABLE} is not set in the session environment. Export it, or pass it with --env {KEY_VARIABLE}=<key>"
        ))
    }
}

pub(crate) fn service_host(session: &Session) -> Option<String> {
    match session
        .env
        .get(OsStr::new(BASE_URL_VARIABLE))
        .filter(|value| !value.is_empty())
    {
        Some(url) => url_host(&url.to_string_lossy()),
        None => Some(SERVICE_HOST.to_string()),
    }
}

pub(crate) fn login_outcome(line: &Value) -> Option<Result<(), String>> {
    match (line["type"].as_str(), line["phase"].as_str()) {
        (Some("status"), Some("step_end")) if line["usage"].is_object() => Some(Ok(())),
        (Some("status"), Some("turn_end")) if line["reason"]["kind"] == ERROR_KIND => {
            Some(Err(error_text(&line["reason"]["error"])))
        }
        (Some("error"), _) => Some(Err(string(&line["message"]))),
        _ => None,
    }
}

fn error_text(error: &Value) -> String {
    let message = string(&error["message"]);
    match error["code"].as_str() {
        Some(code) if message.is_empty() => code.to_string(),
        Some(code) => format!("{code}: {message}"),
        None => message,
    }
}

fn patch_entries(args: &RunArgs) -> Value {
    let mut entries: Vec<Value> = DISABLED_ENTRIES
        .iter()
        .chain(if args.no_subagents {
            SUBAGENT_ENTRIES.as_slice()
        } else {
            &[]
        })
        .map(|id| json!({"id": id, "disabled": true}))
        .collect();
    if let Some(model) = &args.model {
        entries.push(json!({
            "id": MODEL_ENTRY,
            "config": {"provider": MODEL_PROVIDER, "model": model}
        }));
    }
    if let Some(effort) = &args.effort {
        entries.push(json!({"id": EFFORT_ENTRY, "config": {"reasoningEffort": effort}}));
    }
    Value::Array(entries)
}

fn write_patch(path: &Path, args: &RunArgs) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(&patch_entries(args)).unwrap_or_default();
    text.push('\n');
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|error| {
            format!(
                "cannot write the dsh patch file {}: {error}",
                path.display()
            )
        })
}

fn create_private_dir(dir: &Path) -> Result<(), String> {
    DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .map_err(|error| format!("cannot create the dsh directory {}: {error}", dir.display()))
}

fn project_key(cwd: &str) -> String {
    let mut readable = String::new();
    let mut separator_run = false;
    for unit in cwd.encode_utf16() {
        let ch = char::from_u32(u32::from(unit));
        match ch {
            Some('/' | '\\' | ':') => {
                if !separator_run {
                    readable.push('-');
                }
                separator_run = true;
                continue;
            }
            Some(ch) if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') => {
                readable.push(ch)
            }
            _ => readable.push_str(&format!("~{unit:04X}")),
        }
        separator_run = false;
    }
    let trimmed = readable.trim_start_matches('-');
    let body = if trimmed.is_empty() { "root" } else { trimmed };
    let body: String = body.chars().take(PROJECT_KEY_MAX).collect();
    format!("--{body}--")
}

fn counts(usage: &Value) -> TokenCounts {
    TokenCounts {
        input_tokens: usage["inputTokens"].as_u64(),
        output_tokens: usage["outputTokens"].as_u64(),
        cache_read_tokens: usage["cacheReadTokens"].as_u64(),
        cache_write_tokens: usage["cacheWriteTokens"].as_u64(),
        cost_usd: None,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;
    use crate::cli::{SandboxMode, parse_run};
    use crate::network::ProxyEndpoint;
    use crate::sandbox::{Sandbox, Wrapper};
    use crate::session::{Session, parse_env_args};
    use crate::transcript::Saved;

    const SAVED_ID: &str = "bd2ebf55-0917-4915-8793-20a5fc640cb9";

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

        fn invocation(&self, env: &[(&str, &str)], sandboxed: bool, extra: &[&str]) -> Invocation {
            let mut args = vec!["agentrun", "dsh", "--prompt", "hi"];
            args.extend(extra);
            let (runtime, args) = parse_run(&args);
            let caller_env: Vec<(OsString, OsString)> = env
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                .collect();
            let env_args = parse_env_args(&args.env, &caller_env).unwrap();
            let session = Session::assemble(runtime, &caller_env, &[], &[], &env_args);
            let wrapper = if sandboxed {
                Wrapper::Bubblewrap {
                    bwrap: PathBuf::from("/usr/bin/bwrap"),
                    socat: PathBuf::from("/usr/bin/socat"),
                }
            } else {
                Wrapper::None
            };
            Invocation {
                sandbox: Sandbox::new(SandboxMode::On, wrapper),
                tempdir: self.tempdir(),
                ..Invocation::new(runtime, args, self.work(), "hi".to_string(), session)
            }
        }
    }

    const KEY: (&str, &str) = ("DEEPSEEK_API_KEY", "sk-test");

    fn launch(setup: &Setup, extra: &[&str]) -> (Dsh, Launch) {
        let mut adapter = Dsh::new();
        let launch = adapter
            .launch(
                Path::new("/opt/bin/dsh"),
                &setup.invocation(&[KEY], false, extra),
            )
            .unwrap();
        (adapter, launch)
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| item.to_string()).collect()
    }

    fn env_value(launch: &Launch, key: &str) -> Option<String> {
        launch
            .env
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.to_string_lossy().into_owned())
    }

    fn patch(setup: &Setup) -> Value {
        serde_json::from_str(&std::fs::read_to_string(setup.tempdir().join(PATCH_FILE)).unwrap())
            .unwrap()
    }

    fn disabled(ids: &[&str]) -> Vec<Value> {
        ids.iter()
            .map(|id| json!({"id": id, "disabled": true}))
            .collect()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn arguments_environment_and_private_dirs_without_options() {
        let setup = Setup::new();
        let (_, launch) = launch(&setup, &[]);
        let patch_path = setup.tempdir().join(PATCH_FILE);
        assert_eq!(
            launch.argv,
            strings(&[
                "/opt/bin/dsh",
                "headless",
                "--patch",
                patch_path.to_str().unwrap(),
                "--json",
                "-",
            ])
        );
        assert_eq!(launch.stdin, b"hi");
        assert!(!launch.signal_wrapped_child);
        assert_eq!(launch.wrapped, SandboxKind::None);
        assert!(launch.service_hosts.is_empty());
        let home = setup.tempdir().join(HOME_SUBDIR);
        let agents = setup.tempdir().join(AGENTS_SUBDIR);
        assert_eq!(
            launch.env,
            [
                ("DSH_HOME", home.to_str().unwrap()),
                ("DSH_AGENTS_HOME", agents.to_str().unwrap()),
                ("DSH_PERMISSION_MODE", "danger-full-access"),
                ("DSH_TELEMETRY_DISABLED", "1"),
            ]
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        );
        for dir in [&home, &agents] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
            assert_eq!(mode(dir), 0o700);
        }
        assert_eq!(mode(&patch_path), 0o600);
        assert_eq!(patch(&setup), Value::Array(disabled(&DISABLED_ENTRIES)));
    }

    #[test]
    fn model_effort_and_no_subagents_go_into_the_patch() {
        let setup = Setup::new();
        let (_, launch) = launch(
            &setup,
            &[
                "--model",
                "deepseek-v4-pro",
                "--effort",
                "max \"x\"",
                "--no-subagents",
                "--",
                "--verbose",
            ],
        );
        assert_eq!(launch.argv[5..], strings(&["--verbose", "-"]));
        let mut expected = disabled(&DISABLED_ENTRIES);
        expected.extend(disabled(&SUBAGENT_ENTRIES));
        expected.push(json!({"id": "agent-default-model", "config": {"provider": "deepseek-official", "model": "deepseek-v4-pro"}}));
        expected.push(json!({"id": "llm-deepseek", "config": {"reasoningEffort": "max \"x\""}}));
        assert_eq!(patch(&setup), Value::Array(expected));
    }

    #[test]
    fn dry_run_writes_nothing() {
        let setup = Setup::new();
        let (_, launch) = launch(&setup, &["--dry-run"]);
        assert_eq!(launch.argv.last().unwrap(), "-");
        assert_eq!(std::fs::read_dir(setup.tempdir()).unwrap().count(), 0);
    }

    #[test]
    fn missing_key_fails_the_launch_even_in_a_dry_run() {
        let setup = Setup::new();
        for (env, extra) in [
            (vec![], vec![]),
            (vec![("DEEPSEEK_API_KEY", "")], vec![]),
            (vec![], vec!["--dry-run"]),
        ] {
            let detail = Dsh::new()
                .launch(
                    Path::new("/opt/bin/dsh"),
                    &setup.invocation(&env, false, &extra),
                )
                .unwrap_err();
            assert_eq!(
                detail,
                "DEEPSEEK_API_KEY is not set in the session environment. Export it, or pass it with --env DEEPSEEK_API_KEY=<key>"
            );
        }
        assert_eq!(std::fs::read_dir(setup.tempdir()).unwrap().count(), 0);
        let given = setup.invocation(&[], false, &["--env", "DEEPSEEK_API_KEY=k"]);
        assert!(check_key(&given.session).is_ok());
    }

    #[test]
    fn caller_dsh_variables_are_removed_from_the_session() {
        let setup = Setup::new();
        let invocation = setup.invocation(
            &[
                KEY,
                ("DSH_HOME", "/home/me/.dsh"),
                ("DSH_TOOLS_MODE", "x"),
                ("DSHX", "1"),
            ],
            false,
            &[],
        );
        assert_eq!(invocation.session.removed, ["DSH_HOME", "DSH_TOOLS_MODE"]);
        assert!(invocation.session.env.contains_key(OsStr::new("DSHX")));
    }

    #[test]
    fn check_args_accepts_limits_and_rejects_images() {
        let (_, args) = parse_run(&["agentrun", "dsh", "--max-turns", "2"]);
        assert_eq!(Dsh::new().check_args(&args), Ok(()));
        let (_, args) = parse_run(&["agentrun", "dsh", "--max-tokens", "1000"]);
        assert_eq!(Dsh::new().check_args(&args), Ok(()));
        let (_, args) = parse_run(&["agentrun", "dsh", "--image", "a.png"]);
        assert_eq!(Dsh::new().check_args(&args), Err(NO_IMAGES.to_string()));
        let (_, args) = parse_run(&["agentrun", "dsh", "--no-subagents"]);
        assert_eq!(Dsh::new().check_args(&args), Ok(()));
    }

    fn proxy(setup: &Setup) -> ProxyEndpoint {
        ProxyEndpoint {
            port: Some(4321),
            socket: setup.tempdir().join("proxy.sock"),
        }
    }

    fn proxied(setup: &Setup, env: &[(&str, &str)]) -> Result<Launch, String> {
        let mut invocation = setup.invocation(env, true, &[]);
        invocation.proxy = Some(proxy(setup));
        Dsh::new().launch(Path::new("/opt/bin/dsh"), &invocation)
    }

    #[test]
    fn sandboxed_launch_wraps_dsh_and_forwards_through_the_proxy() {
        let setup = Setup::new();
        let launch = proxied(&setup, &[KEY]).unwrap();
        assert_eq!(launch.argv[0], "/usr/bin/bwrap");
        assert!(launch.signal_wrapped_child);
        assert_eq!(launch.wrapped, SandboxKind::Bubblewrap);
        assert_eq!(launch.argv.iter().filter(|arg| *arg == "--bind").count(), 2);
        let start = launch
            .argv
            .iter()
            .position(|arg| arg == "/opt/bin/dsh")
            .unwrap();
        assert_eq!(
            launch.argv[start - 3..start - 1],
            ["/usr/bin/socat", "4321"]
        );
        assert_eq!(launch.service_hosts, ["api.deepseek.com"]);
        assert_eq!(
            env_value(&launch, "HTTPS_PROXY").as_deref(),
            Some("http://127.0.0.1:4321")
        );
        assert_eq!(
            env_value(&launch, "DSH_PERMISSION_MODE").as_deref(),
            Some("danger-full-access")
        );
    }

    #[test]
    fn base_url_decides_the_service_host() {
        let setup = Setup::new();
        let launch = proxied(
            &setup,
            &[
                KEY,
                ("DEEPSEEK_BASE_URL", "https://user@Gateway.example:8443/v1"),
            ],
        )
        .unwrap();
        assert_eq!(launch.service_hosts, ["gateway.example"]);
        let setup = Setup::new();
        let detail = proxied(&setup, &[KEY, ("DEEPSEEK_BASE_URL", "gateway")]).unwrap_err();
        assert_eq!(
            detail,
            "cannot tell which host dsh's model service uses from DEEPSEEK_BASE_URL. Use --network custom --allow-host <host of the model service>"
        );
        let setup = Setup::new();
        let mut invocation = setup.invocation(
            &[KEY, ("DEEPSEEK_BASE_URL", "gateway")],
            true,
            &["--network", "full"],
        );
        invocation.proxy = Some(proxy(&setup));
        let launch = Dsh::new()
            .launch(Path::new("/opt/bin/dsh"), &invocation)
            .unwrap();
        assert!(launch.service_hosts.is_empty());
    }

    #[test]
    fn project_key_matches_dsh() {
        let long = format!("/{}", "a".repeat(300));
        let cases = [
            ("/tmp/claude-0/x/w", "--tmp-claude-0-x-w--".to_string()),
            ("/", "--root--".to_string()),
            ("/a//b/", "--a-b---".to_string()),
            (
                "/home/me/修复 dir~x",
                "--home-me-~4FEE~590D~0020dir~007Ex--".to_string(),
            ),
            ("/x/😀", "--x-~D83D~DE00--".to_string()),
            ("/-a-/b", "--a--b--".to_string()),
            (long.as_str(), format!("--{}--", "a".repeat(251))),
        ];
        for (cwd, key) in cases {
            assert_eq!(project_key(cwd), key, "{cwd}");
        }
    }

    fn translate(adapter: &mut Dsh, line: Value) -> Vec<Record> {
        adapter.translate(&line)
    }

    #[test]
    fn events_become_records() {
        let setup = Setup::new();
        let (mut adapter, _) = launch(&setup, &["--model", "deepseek-flash"]);
        let work = std::fs::canonicalize(setup.work()).unwrap();
        let file = work.join("src/main.rs");
        let none: Vec<Record> = Vec::new();
        assert_eq!(
            translate(
                &mut adapter,
                json!({"type": "session", "sessionId": "session-1", "cwd": "/w"})
            ),
            none
        );
        assert_eq!(
            translate(&mut adapter, json!({"type": "thinking", "text": "plan"})),
            none
        );
        assert_eq!(
            translate(&mut adapter, json!({"type": "text", "text": ""})),
            none
        );
        assert_eq!(
            translate(&mut adapter, json!({"type": "text", "text": "hello"})),
            vec![Record::Text {
                parent: None,
                text: "hello".to_string()
            }]
        );
        assert_eq!(
            translate(
                &mut adapter,
                json!({"type": "tool_call", "callId": "c1", "tool": "edit", "input": {"file_path": file}})
            ),
            vec![Record::ToolStart {
                id: "c1".to_string(),
                parent: None,
                name: "edit".to_string(),
                summary: "edit: src/main.rs".to_string()
            }]
        );
        for (status, error) in [
            ("completed", Some(false)),
            ("error", Some(true)),
            ("other", None),
        ] {
            assert_eq!(
                translate(
                    &mut adapter,
                    json!({"type": "tool_result", "callId": "c1", "status": status, "result": "x"})
                ),
                vec![Record::ToolEnd {
                    id: "c1".to_string(),
                    denied: false,
                    error
                }]
            );
        }
        for (result, error) in [
            (json!("ok"), Some(false)),
            (json!("out\n[exit code: 0]"), Some(false)),
            (
                json!("[stderr]\nRead-only file system\n[exit code: 1]"),
                Some(true),
            ),
            (json!("000\n[exit code: 56]"), Some(true)),
            (json!("partial\n[killed by signal: SIGTERM]"), Some(true)),
            (json!("echo '\n[exit code: 3]' done"), Some(false)),
            (json!({"text": "x"}), None),
        ] {
            translate(
                &mut adapter,
                json!({"type": "tool_call", "callId": "s1", "tool": "bash", "input": {"command": "make"}}),
            );
            assert_eq!(
                translate(
                    &mut adapter,
                    json!({"type": "tool_result", "callId": "s1", "status": "completed", "result": result})
                ),
                vec![Record::ToolEnd {
                    id: "s1".to_string(),
                    denied: false,
                    error
                }],
                "{result}"
            );
        }
        translate(
            &mut adapter,
            json!({"type": "tool_call", "callId": "r1", "tool": "read", "input": {"file_path": "log"}}),
        );
        assert_eq!(
            translate(
                &mut adapter,
                json!({"type": "tool_result", "callId": "r1", "status": "completed", "result": "x\n[exit code: 1]"})
            ),
            vec![Record::ToolEnd {
                id: "r1".to_string(),
                denied: false,
                error: Some(false)
            }]
        );
        assert_eq!(
            translate(
                &mut adapter,
                json!({"type": "status", "phase": "step_start", "turn": 1, "step": 1})
            ),
            vec![Record::CallStart { parent: None }]
        );
        assert_eq!(
            translate(
                &mut adapter,
                json!({"type": "status", "phase": "step_end", "turn": 1, "step": 1, "usage": {"inputTokens": 5, "outputTokens": 6, "cacheReadTokens": 7, "cacheWriteTokens": 0, "totalTokens": 18}})
            ),
            vec![Record::Usage {
                parent: None,
                model: Some("deepseek-flash".to_string()),
                counts: TokenCounts {
                    input_tokens: Some(5),
                    output_tokens: Some(6),
                    cache_read_tokens: Some(7),
                    cache_write_tokens: Some(0),
                    cost_usd: None,
                }
            }]
        );
        assert_eq!(
            translate(
                &mut adapter,
                json!({"type": "status", "phase": "step_end", "turn": 1, "step": 2})
            ),
            none
        );
        assert_eq!(
            translate(&mut adapter, json!({"type": "final", "text": ""})),
            none
        );
        assert_eq!(
            translate(&mut adapter, json!({"type": "final", "text": "done"})),
            vec![Record::Result {
                text: "done".to_string()
            }]
        );
    }

    #[test]
    fn summary_per_tool_and_missing_fields() {
        let mut adapter = Dsh::new();
        adapter.cwd = PathBuf::from("/w");
        let cases = [
            ("bash", json!({"command": "ls\npwd"}), "bash: ls"),
            ("read", json!({"file_path": "/w/a.txt"}), "read: a.txt"),
            (
                "write",
                json!({"file_path": "/elsewhere/b"}),
                "write: /elsewhere/b",
            ),
            (
                "read_image",
                json!({"file_path": "/w/i.png"}),
                "read_image: i.png",
            ),
            ("edit", json!({"file_path": "/w"}), "edit: ."),
            ("glob", json!({"pattern": "**/*.rs"}), "glob: **/*.rs"),
            ("grep", json!({"pattern": "fn main"}), "grep: fn main"),
            ("bash", json!({}), "bash"),
            ("todo_write", json!({"todos": []}), "todo_write"),
        ];
        for (name, input, summary) in cases {
            assert_eq!(adapter.tool_summary(name, &input), summary, "{name}");
        }
    }

    fn failure(lines: &[Value], exit_code: Option<i32>) -> Option<Failure> {
        let mut adapter = Dsh::new();
        for line in lines {
            adapter.translate(line);
        }
        adapter.failure(exit_code)
    }

    fn turn_end(reason: Value) -> Value {
        json!({"type": "status", "phase": "turn_end", "turn": 1, "reason": reason})
    }

    #[test]
    fn failure_cases_in_order() {
        let completed = turn_end(json!({"kind": "completed"}));
        let auth = turn_end(
            json!({"kind": "error", "error": {"code": "AUTH", "message": "bad key", "status": 401}}),
        );
        let startup = json!({"type": "error", "message": "unknown option '--x'"});
        assert_eq!(failure(std::slice::from_ref(&completed), Some(0)), None);
        assert_eq!(
            failure(&[auth.clone(), startup.clone()], Some(1)),
            Some(Failure::Message("unknown option '--x'".to_string()))
        );
        assert_eq!(
            failure(&[auth], Some(1)),
            Some(Failure::Message("AUTH: bad key".to_string()))
        );
        assert_eq!(
            failure(
                &[turn_end(
                    json!({"kind": "error", "error": {"code": "TRANSPORT"}})
                )],
                Some(1)
            ),
            Some(Failure::Message("TRANSPORT".to_string()))
        );
        assert_eq!(
            failure(&[turn_end(json!({"kind": "error", "error": {}}))], Some(1)),
            Some(Failure::Unexplained)
        );
        assert_eq!(
            failure(&[turn_end(json!({"kind": "max-tokens"}))], Some(1)),
            Some(Failure::Message("turn_end reason: max-tokens".to_string()))
        );
        let long = format!("{}{}", "a".repeat(400), "b".repeat(200));
        assert_eq!(
            failure(&[json!({"type": "error", "message": long})], Some(1)),
            Some(Failure::Message(format!(
                "{}{}",
                "a".repeat(400),
                "b".repeat(100)
            )))
        );
        assert_eq!(failure(&[completed], Some(1)), Some(Failure::Unexplained));
        assert_eq!(failure(&[], Some(1)), Some(Failure::Unexplained));
        assert_eq!(
            failure(&[], Some(0)),
            Some(Failure::Message(NO_TURN_END_DETAIL.to_string()))
        );
    }

    #[test]
    fn login_outcome_cases() {
        let cases = [
            (
                json!({"type": "status", "phase": "step_end", "usage": {"inputTokens": 1}}),
                Some(Ok(())),
            ),
            (json!({"type": "status", "phase": "step_end"}), None),
            (
                turn_end(
                    json!({"kind": "error", "error": {"code": "MISSING_CREDENTIAL", "message": "no key"}}),
                ),
                Some(Err("MISSING_CREDENTIAL: no key".to_string())),
            ),
            (turn_end(json!({"kind": "completed"})), None),
            (
                json!({"type": "error", "message": "a task is required"}),
                Some(Err("a task is required".to_string())),
            ),
            (json!({"type": "text", "text": "OK"}), None),
        ];
        for (line, outcome) in cases {
            assert_eq!(login_outcome(&line), outcome, "{line}");
        }
    }

    fn saved_dir(setup: &Setup) -> PathBuf {
        let dir = setup
            .root
            .path()
            .join("store/dsh")
            .join(format!("session-{SAVED_ID}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.v4.jsonl.zstd"), "log").unwrap();
        std::fs::write(dir.join("session.lock"), "").unwrap();
        dir
    }

    fn resumed(setup: &Setup, extra: &[&str]) -> (Dsh, Launch) {
        let file = saved_dir(setup);
        let mut invocation = setup.invocation(
            &[KEY],
            false,
            &[&["--session-dir", "store"], extra].concat(),
        );
        invocation.resume = Some(Saved {
            id: SAVED_ID.to_string(),
            file,
        });
        let mut adapter = Dsh::new();
        let launch = adapter
            .launch(Path::new("/opt/bin/dsh"), &invocation)
            .unwrap();
        (adapter, launch)
    }

    #[test]
    fn resume_copies_the_saved_session_under_the_project_key_of_the_cwd() {
        let setup = Setup::new();
        let (adapter, launch) = resumed(&setup, &[]);
        assert_eq!(
            launch.argv[5..],
            strings(&["--session-id", &format!("session-{SAVED_ID}"), "-"])
        );
        let work = std::fs::canonicalize(setup.work()).unwrap();
        let copied = setup
            .tempdir()
            .join(HOME_SUBDIR)
            .join(SESSIONS_SUBDIR)
            .join(project_key(work.to_str().unwrap()))
            .join(format!("session-{SAVED_ID}"));
        assert_eq!(
            std::fs::read_to_string(copied.join("session.v4.jsonl.zstd")).unwrap(),
            "log"
        );
        let transcript = adapter.transcript().unwrap();
        assert_eq!(transcript.id, SAVED_ID);
        assert_eq!(transcript.entries, [copied]);
    }

    #[test]
    fn dry_run_resume_copies_nothing() {
        let setup = Setup::new();
        let (_, launch) = resumed(&setup, &["--dry-run"]);
        assert!(launch.argv.contains(&format!("session-{SAVED_ID}")));
        assert!(!setup.tempdir().join(HOME_SUBDIR).exists());
    }

    #[test]
    fn transcript_of_a_new_session_follows_the_session_event() {
        let setup = Setup::new();
        let (mut saving, _) = launch(&setup, &["--session-dir", "store"]);
        let dir = setup
            .tempdir()
            .join(HOME_SUBDIR)
            .join(SESSIONS_SUBDIR)
            .join("--any--")
            .join(format!("session-{SAVED_ID}"));
        assert_eq!(saving.transcript(), None);
        saving.translate(&json!({"type": "session", "sessionId": format!("session-{SAVED_ID}")}));
        assert_eq!(saving.transcript(), None);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            saving.transcript(),
            Some(Transcript {
                id: SAVED_ID.to_string(),
                entries: vec![dir],
                emptied_dir: None,
            })
        );
        let setup = Setup::new();
        let (mut unsaved, _) = launch(&setup, &[]);
        unsaved.translate(&json!({"type": "session", "sessionId": format!("session-{SAVED_ID}")}));
        assert_eq!(unsaved.transcript(), None);
    }

    #[test]
    fn saved_session_is_the_directory_named_after_the_id() {
        let setup = Setup::new();
        let dir = saved_dir(&setup);
        let store = dir.parent().unwrap();
        assert_eq!(saved_session(store, SAVED_ID), Some(dir.clone()));
        assert_eq!(
            saved_session(store, "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"),
            None
        );
        std::fs::write(
            store.join("session-0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"),
            "",
        )
        .unwrap();
        assert_eq!(
            saved_session(store, "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"),
            None
        );
        let linked = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5c";
        std::os::unix::fs::symlink(&dir, store.join(format!("session-{linked}"))).unwrap();
        assert_eq!(saved_session(store, linked), None);
    }

    #[test]
    fn transcript_ignores_symbolic_links_in_the_sessions_directory() {
        let setup = Setup::new();
        let (mut saving, _) = launch(&setup, &["--session-dir", "store"]);
        saving.translate(&json!({"type": "session", "sessionId": format!("session-{SAVED_ID}")}));
        let sessions = setup.tempdir().join(HOME_SUBDIR).join(SESSIONS_SUBDIR);
        let outside = setup.root.path().join("outside");
        std::fs::create_dir_all(outside.join(format!("session-{SAVED_ID}"))).unwrap();
        std::fs::create_dir_all(sessions.join("--linked-session--")).unwrap();
        std::os::unix::fs::symlink(
            outside.join(format!("session-{SAVED_ID}")),
            sessions
                .join("--linked-session--")
                .join(format!("session-{SAVED_ID}")),
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, sessions.join("--linked-project--")).unwrap();
        assert_eq!(saving.transcript(), None);
    }
}
