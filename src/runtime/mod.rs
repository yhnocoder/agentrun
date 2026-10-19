pub mod claudecode;
pub mod codex;
pub mod pi;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cli::{Format, RunArgs, Runtime, SandboxMode};
use crate::network::{HostRule, ProxyEndpoint};
use crate::output::{Record, SandboxKind};
use crate::sandbox::{PiState, Sandbox, Wrapper};
use crate::session::Session;
use claudecode::ClaudeCode;
use codex::Codex;
use pi::Pi;

pub const DETAIL_MAX_CHARS: usize = 500;
pub const DENIED_HOME_ENTRIES: [&str; 9] = [
    ".ssh",
    ".aws",
    ".gnupg",
    ".config/gh",
    ".netrc",
    ".kube",
    ".docker/config.json",
    ".config/gcloud",
    ".azure",
];
const RUNTIMES: [Runtime; 3] = [Runtime::ClaudeCode, Runtime::Codex, Runtime::Pi];

pub trait Adapter {
    fn runtime(&self) -> Runtime;
    fn check_args(&self, args: &RunArgs) -> Result<(), String>;
    fn launch(&mut self, executable: &Path, invocation: &Invocation) -> Result<Launch, String>;
    fn echoes_prompt(&self) -> bool;
    fn translate(&mut self, line: &Value) -> Vec<Record>;
    fn after_exit(&mut self) -> Vec<Record>;
    fn failure(&self, exit_code: Option<i32>) -> Option<Failure>;
}

pub fn builtin(runtime: Runtime) -> Box<dyn Adapter> {
    match runtime {
        Runtime::ClaudeCode => Box::new(ClaudeCode::new()),
        Runtime::Pi => Box::new(Pi::new()),
        Runtime::Codex => Box::new(Codex::new()),
    }
}

pub(crate) fn model_service_host(
    runtime: Runtime,
    model: Option<&str>,
    session: &Session,
    cwd: &Path,
) -> Option<String> {
    match runtime {
        Runtime::ClaudeCode => Some(claudecode::SERVICE_HOST.to_string()),
        Runtime::Codex => Some(codex::SERVICE_HOST.to_string()),
        Runtime::Pi => pi::model_service(model, pi::user_state_dir(session, cwd).as_deref())
            .host
            .map(str::to_string),
    }
}

pub(crate) fn service_hosts(
    runtime: Runtime,
    model: Option<&str>,
    session: &Session,
    cwd: &Path,
) -> Vec<String> {
    match runtime {
        Runtime::ClaudeCode => Vec::new(),
        Runtime::Codex => codex::SERVICE_HOSTS.map(str::to_string).to_vec(),
        Runtime::Pi => model_service_host(runtime, model, session, cwd)
            .into_iter()
            .collect(),
    }
}

pub(crate) fn sandbox_state(
    runtime: Runtime,
    session: &Session,
    cwd: &Path,
) -> Result<Option<PiState>, String> {
    match runtime {
        Runtime::Pi => pi::session_state(pi::user_state_dir(session, cwd).as_deref(), false),
        Runtime::ClaudeCode | Runtime::Codex => Ok(None),
    }
}

pub(crate) fn login_file(runtime: Runtime, session: &Session, cwd: &Path) -> Option<PathBuf> {
    match runtime {
        Runtime::ClaudeCode => claudecode::login_file(session, cwd),
        Runtime::Codex => codex::user_home(session, cwd).map(|home| home.join(codex::LOGIN_FILE)),
        Runtime::Pi => pi::user_state_dir(session, cwd).map(|dir| dir.join(pi::LOGIN_FILE)),
    }
}

fn reads_login_inside_sandbox(runtime: Runtime) -> bool {
    runtime == Runtime::Pi
}

pub fn default_denied_reads(runtime: Runtime, session: &Session, cwd: &Path) -> Vec<PathBuf> {
    let home = session
        .env
        .get(OsStr::new("HOME"))
        .filter(|home| !home.is_empty())
        .map(|home| cwd.join(home));
    let mut paths: Vec<PathBuf> = home
        .iter()
        .flat_map(|home| DENIED_HOME_ENTRIES.iter().map(|entry| home.join(entry)))
        .collect();
    paths.extend(
        RUNTIMES
            .into_iter()
            .filter(|other| *other != runtime || !reads_login_inside_sandbox(runtime))
            .filter_map(|other| login_file(other, session, cwd)),
    );
    paths.retain(|path| !contains(path, cwd));
    paths
}

pub(crate) fn denied_reads(
    runtime: Runtime,
    args: &RunArgs,
    session: &Session,
    cwd: &Path,
) -> Result<Vec<PathBuf>, String> {
    let mut paths = if args.no_default_deny_read {
        Vec::new()
    } else {
        default_denied_reads(runtime, session, cwd)
    };
    for given in &args.deny_read {
        let path = std::path::absolute(given)
            .map_err(|error| format!("--deny-read {}: {error}", given.display()))?;
        if contains(&path, cwd) {
            return Err(format!(
                "--deny-read {}: contains the working directory {}",
                given.display(),
                cwd.display()
            ));
        }
        paths.push(path);
    }
    let mut unique = Vec::new();
    for path in paths {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    Ok(unique)
}

fn contains(path: &Path, cwd: &Path) -> bool {
    if cwd.starts_with(path) {
        return true;
    }
    match (std::fs::canonicalize(path), std::fs::canonicalize(cwd)) {
        (Ok(path), Ok(cwd)) => cwd.starts_with(path),
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    Message(String),
    Unexplained,
}

impl Failure {
    pub fn from_detail(detail: String) -> Failure {
        if detail.is_empty() {
            Failure::Unexplained
        } else {
            Failure::Message(detail)
        }
    }
}

pub fn reject_max_turns(args: &RunArgs) -> Result<(), String> {
    match args.max_turns {
        Some(_) => Err("--max-turns is only supported by claude-code".to_string()),
        None => Ok(()),
    }
}

pub fn detail_head(text: &str) -> String {
    text.chars().take(DETAIL_MAX_CHARS).collect()
}

pub fn detail_tail(text: &str) -> String {
    let skip = text.chars().count().saturating_sub(DETAIL_MAX_CHARS);
    text.chars().skip(skip).collect()
}

pub struct Invocation {
    pub runtime: Runtime,
    pub args: RunArgs,
    pub cwd: PathBuf,
    pub prompt: String,
    pub format: Format,
    pub sandbox: Sandbox,
    pub tempdir: PathBuf,
    pub session: Session,
    pub allow_hosts: Vec<HostRule>,
    pub denied_reads: Vec<PathBuf>,
    pub proxy: Option<ProxyEndpoint>,
}

impl Invocation {
    pub fn new(
        runtime: Runtime,
        args: RunArgs,
        cwd: PathBuf,
        prompt: String,
        session: Session,
    ) -> Invocation {
        Invocation {
            runtime,
            args,
            cwd,
            prompt,
            format: Format::Jsonl,
            sandbox: Sandbox::new(SandboxMode::Off, Wrapper::None),
            tempdir: PathBuf::new(),
            session,
            allow_hosts: Vec::new(),
            denied_reads: Vec::new(),
            proxy: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateDir {
    pub label: &'static str,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub argv: Vec<String>,
    pub stdin: Vec<u8>,
    pub env: Vec<(OsString, OsString)>,
    pub signal_wrapped_child: bool,
    pub service_hosts: Vec<String>,
    pub wrapped: SandboxKind,
    pub private_dirs: Vec<PrivateDir>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::parse_run;

    fn session(runtime: Runtime, state_dir: Option<&Path>) -> Session {
        let env: Vec<(OsString, OsString)> = state_dir
            .map(|dir| {
                (
                    OsString::from("PI_CODING_AGENT_DIR"),
                    dir.as_os_str().to_owned(),
                )
            })
            .into_iter()
            .collect();
        Session::assemble(runtime, &env, &[], &[], &[])
    }

    #[test]
    fn model_service_host_and_service_hosts_per_runtime() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path();
        let claude_code = session(Runtime::ClaudeCode, None);
        let codex = session(Runtime::Codex, None);
        assert_eq!(
            model_service_host(Runtime::ClaudeCode, None, &claude_code, cwd),
            Some("api.anthropic.com".to_string())
        );
        assert!(service_hosts(Runtime::ClaudeCode, None, &claude_code, cwd).is_empty());
        assert_eq!(
            model_service_host(Runtime::Codex, None, &codex, cwd),
            Some("chatgpt.com".to_string())
        );
        assert_eq!(
            service_hosts(Runtime::Codex, None, &codex, cwd),
            [
                "chatgpt.com",
                "ab.chatgpt.com",
                "auth.openai.com",
                "api.openai.com"
            ]
        );

        let state_dir = root.path().join("pi");
        std::fs::create_dir(&state_dir).unwrap();
        let pi = session(Runtime::Pi, Some(&state_dir));
        let cases = [
            (Some("deepseek/deepseek-v4-flash"), Some("api.deepseek.com")),
            (Some("deepseek"), None),
            (None, None),
        ];
        for (model, host) in cases {
            assert_eq!(
                model_service_host(Runtime::Pi, model, &pi, cwd),
                host.map(str::to_string),
                "{model:?}"
            );
            assert_eq!(
                service_hosts(Runtime::Pi, model, &pi, cwd),
                host.map(str::to_string).into_iter().collect::<Vec<_>>(),
                "{model:?}"
            );
        }
        std::fs::write(
            state_dir.join("settings.json"),
            r#"{"defaultProvider":"openrouter"}"#,
        )
        .unwrap();
        assert_eq!(
            model_service_host(Runtime::Pi, None, &pi, cwd),
            Some("openrouter.ai".to_string())
        );
        assert_eq!(
            service_hosts(Runtime::Pi, None, &pi, cwd),
            ["openrouter.ai"]
        );
    }

    #[test]
    fn sandbox_state_prepares_only_the_pi_state_dir() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path();
        let state_dir = root.path().join("pi");
        for runtime in [Runtime::ClaudeCode, Runtime::Codex] {
            assert_eq!(
                sandbox_state(runtime, &session(runtime, Some(&state_dir)), cwd),
                Ok(None)
            );
        }
        assert!(!state_dir.exists());
        let state = sandbox_state(Runtime::Pi, &session(Runtime::Pi, Some(&state_dir)), cwd)
            .unwrap()
            .unwrap();
        assert_eq!(state.dir, std::fs::canonicalize(&state_dir).unwrap());
    }

    fn env_session(runtime: Runtime, env: &[(&str, &str)]) -> Session {
        let env: Vec<(OsString, OsString)> = env
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect();
        Session::assemble(runtime, &env, &[], &[], &[])
    }

    fn home_entries(home: &str) -> Vec<PathBuf> {
        DENIED_HOME_ENTRIES
            .iter()
            .map(|entry| Path::new(home).join(entry))
            .collect()
    }

    #[test]
    fn default_denied_reads_list_home_entries_and_every_login_file_but_pis_own() {
        let cwd = Path::new("/nonexistent/work");
        let home = [("HOME", "/nonexistent/home")];
        let logins = [
            (
                Runtime::ClaudeCode,
                "/nonexistent/home/.claude/.credentials.json",
            ),
            (Runtime::Codex, "/nonexistent/home/.codex/auth.json"),
            (Runtime::Pi, "/nonexistent/home/.pi/agent/auth.json"),
        ];
        for (runtime, _) in logins {
            let mut expected = home_entries("/nonexistent/home");
            expected.extend(
                logins
                    .iter()
                    .filter(|(other, _)| runtime != Runtime::Pi || *other != Runtime::Pi)
                    .map(|(_, login)| PathBuf::from(login)),
            );
            assert_eq!(
                default_denied_reads(runtime, &env_session(runtime, &home), cwd),
                expected,
                "{runtime:?}"
            );
        }
        let moved = env_session(
            Runtime::Pi,
            &[
                ("HOME", "/nonexistent/home"),
                ("CLAUDE_CONFIG_DIR", "/nonexistent/claude"),
                ("CODEX_HOME", "relative/codex"),
            ],
        );
        let paths = default_denied_reads(Runtime::Pi, &moved, cwd);
        assert_eq!(
            paths[DENIED_HOME_ENTRIES.len()..],
            [
                PathBuf::from("/nonexistent/claude/.credentials.json"),
                PathBuf::from("/nonexistent/work/relative/codex/auth.json"),
            ]
        );
        assert_eq!(
            default_denied_reads(Runtime::Codex, &env_session(Runtime::Codex, &[]), cwd),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn default_denied_reads_leave_out_paths_that_contain_the_working_directory() {
        let session = env_session(Runtime::Pi, &[("HOME", "/nonexistent/home")]);
        let paths = default_denied_reads(
            Runtime::Pi,
            &session,
            Path::new("/nonexistent/home/.ssh/project"),
        );
        assert!(!paths.contains(&PathBuf::from("/nonexistent/home/.ssh")));
        assert!(paths.contains(&PathBuf::from("/nonexistent/home/.aws")));
    }

    #[test]
    fn denied_reads_add_the_given_paths_after_the_defaults() {
        let cwd = std::env::current_dir().unwrap();
        let session = env_session(Runtime::Pi, &[("HOME", "/nonexistent/home")]);
        let parse = |extra: &[&str]| {
            let mut argv = vec!["agentrun", "pi", "--prompt", "hi"];
            argv.extend(extra);
            parse_run(&argv).1
        };
        let given = parse(&[
            "--deny-read",
            "secrets",
            "--deny-read",
            "/nonexistent/home/.ssh",
        ]);
        let paths = denied_reads(Runtime::Pi, &given, &session, &cwd).unwrap();
        assert_eq!(
            paths.len(),
            default_denied_reads(Runtime::Pi, &session, &cwd).len() + 1
        );
        assert_eq!(paths.last(), Some(&cwd.join("secrets")));
        let only = parse(&["--no-default-deny-read", "--deny-read", "/nonexistent/x"]);
        assert_eq!(
            denied_reads(Runtime::Pi, &only, &session, &cwd).unwrap(),
            [PathBuf::from("/nonexistent/x")]
        );
        let parent = cwd.parent().unwrap().to_str().unwrap().to_string();
        let containing = parse(&["--deny-read", &parent]);
        assert_eq!(
            denied_reads(Runtime::Pi, &containing, &session, &cwd),
            Err(format!(
                "--deny-read {parent}: contains the working directory {}",
                cwd.display()
            ))
        );
    }

    #[test]
    fn detail_head_and_tail_keep_500_characters() {
        let text = format!("{}{}", "a".repeat(300), "b".repeat(300));
        assert_eq!(
            detail_head(&text),
            format!("{}{}", "a".repeat(300), "b".repeat(200))
        );
        assert_eq!(
            detail_tail(&text),
            format!("{}{}", "a".repeat(200), "b".repeat(300))
        );
        let multibyte = format!("{}修复", "a".repeat(499));
        assert_eq!(detail_head(&multibyte), format!("{}修", "a".repeat(499)));
        assert_eq!(detail_tail(&multibyte), format!("{}修复", "a".repeat(498)));
        assert_eq!(detail_head("short"), "short");
        assert_eq!(detail_tail("short"), "short");
    }
}
