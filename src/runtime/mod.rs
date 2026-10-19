pub mod claudecode;
pub mod codex;
pub mod dsh;
pub mod pi;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde_json::Value;

use crate::cli::{Format, NetworkMode, RunArgs, Runtime, SandboxMode};
use crate::image::Image;
use crate::network::{HostRule, ProxyEndpoint};
use crate::output::{Record, SandboxKind};
use crate::sandbox::{PiState, Sandbox, Wrapper};
use crate::session::Session;
use crate::transcript::{Saved, Transcript};
use claudecode::ClaudeCode;
use codex::Codex;
use dsh::Dsh;
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

pub trait Adapter {
    fn runtime(&self) -> Runtime;
    fn check_args(&self, args: &RunArgs) -> Result<(), String>;
    fn launch(&mut self, executable: &Path, invocation: &Invocation) -> Result<Launch, String>;
    fn echoes_prompt(&self) -> bool;
    fn translate(&mut self, line: &Value) -> Vec<Record>;
    fn after_exit(&mut self) -> Vec<Record>;
    fn failure(&self, exit_code: Option<i32>) -> Option<Failure>;
    fn transcript(&self) -> Option<Transcript>;
}

pub fn builtin(runtime: Runtime) -> Box<dyn Adapter> {
    match runtime {
        Runtime::ClaudeCode => Box::new(ClaudeCode::new()),
        Runtime::Pi => Box::new(Pi::new()),
        Runtime::Codex => Box::new(Codex::new()),
        Runtime::Dsh => Box::new(Dsh::new()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version(pub u64, pub u64, pub u64);

impl Version {
    pub(crate) fn find(text: &str) -> Option<Version> {
        text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';'))
            .find_map(Version::parse)
    }

    fn parse(word: &str) -> Option<Version> {
        let word = word.strip_prefix('v').unwrap_or(word);
        let mut parts = word.splitn(3, '.');
        let major = number(parts.next()?)?;
        let minor = number(parts.next()?)?;
        let rest = parts.next()?;
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .map_or(rest, |end| &rest[..end]);
        Some(Version(major, minor, number(digits)?))
    }
}

fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.0, self.1, self.2)
    }
}

pub(crate) fn min_version(runtime: Runtime) -> Version {
    match runtime {
        Runtime::ClaudeCode => claudecode::MIN_VERSION,
        Runtime::Codex => codex::MIN_VERSION,
        Runtime::Pi => pi::MIN_VERSION,
        Runtime::Dsh => dsh::MIN_VERSION,
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
        Runtime::Dsh => dsh::service_host(session),
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
        Runtime::Pi | Runtime::Dsh => model_service_host(runtime, model, session, cwd)
            .into_iter()
            .collect(),
    }
}

pub(crate) fn proxy_needed(runtime: Runtime, mode: NetworkMode, sandboxed: bool) -> bool {
    sandboxed
        && match runtime {
            Runtime::Pi | Runtime::Dsh => true,
            Runtime::ClaudeCode => mode != NetworkMode::None,
            Runtime::Codex => mode == NetworkMode::Custom,
        }
}

pub(crate) fn starts_inside_wrapper(runtime: Runtime) -> bool {
    match runtime {
        Runtime::Pi | Runtime::Dsh => true,
        Runtime::ClaudeCode | Runtime::Codex => false,
    }
}

fn state_dir(runtime: Runtime, session: &Session, cwd: &Path) -> Option<PathBuf> {
    match runtime {
        Runtime::Pi => pi::user_state_dir(session, cwd),
        Runtime::ClaudeCode | Runtime::Codex | Runtime::Dsh => None,
    }
}

pub(crate) fn sandbox_state(
    runtime: Runtime,
    session: &Session,
    cwd: &Path,
) -> Result<Option<PiState>, String> {
    match state_dir(runtime, session, cwd) {
        Some(dir) => pi::session_state(Some(&dir), false),
        None => Ok(None),
    }
}

pub(crate) fn login_file(runtime: Runtime, session: &Session, cwd: &Path) -> Option<PathBuf> {
    match runtime {
        Runtime::ClaudeCode => claudecode::login_file(session, cwd),
        Runtime::Codex => codex::user_home(session, cwd).map(|home| home.join(codex::LOGIN_FILE)),
        Runtime::Pi => pi::user_state_dir(session, cwd).map(|dir| dir.join(pi::LOGIN_FILE)),
        Runtime::Dsh => dsh::user_home(session, cwd).map(|home| home.join(dsh::ENV_FILE)),
    }
}

fn history_paths(runtime: Runtime, session: &Session, cwd: &Path) -> Vec<PathBuf> {
    match runtime {
        Runtime::ClaudeCode => claudecode::history_paths(session, cwd),
        Runtime::Codex => codex::history_paths(session, cwd),
        Runtime::Pi => Vec::new(),
        Runtime::Dsh => dsh::history_paths(session, cwd),
    }
}

fn reads_login_inside_sandbox(runtime: Runtime) -> bool {
    match runtime {
        Runtime::Pi => true,
        Runtime::ClaudeCode | Runtime::Codex | Runtime::Dsh => false,
    }
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
        Runtime::value_variants()
            .iter()
            .filter(|other| **other != runtime || !reads_login_inside_sandbox(runtime))
            .filter_map(|other| login_file(*other, session, cwd)),
    );
    paths.extend(
        Runtime::value_variants()
            .iter()
            .flat_map(|other| history_paths(*other, session, cwd)),
    );
    paths.retain(|path| !contains(path, cwd));
    paths
}

pub(crate) fn denied_reads(
    runtime: Runtime,
    args: &RunArgs,
    session: &Session,
    cwd: &Path,
    temp_base: &Path,
) -> Result<Vec<PathBuf>, String> {
    let mut needed = vec![
        ("working directory".to_string(), cwd.to_path_buf()),
        ("temporary directory".to_string(), temp_base.to_path_buf()),
    ];
    needed.extend(
        state_dir(runtime, session, cwd)
            .map(|dir| (format!("{} state directory", runtime.name()), dir)),
    );
    let mut paths = if args.no_default_deny_read {
        Vec::new()
    } else {
        default_denied_reads(runtime, session, cwd)
    };
    paths.retain(|path| needed.iter().all(|(_, dir)| !contains(path, dir)));
    for given in &args.deny_read {
        if given.to_string_lossy().starts_with('~') {
            return Err(format!(
                "--deny-read {}: ~ is not expanded, give the full path",
                given.display()
            ));
        }
        let path = std::path::absolute(given)
            .map_err(|error| format!("--deny-read {}: {error}", given.display()))?;
        if let Some((name, dir)) = needed.iter().find(|(_, dir)| contains(&path, dir)) {
            return Err(format!(
                "--deny-read {}: contains the {name} {}",
                given.display(),
                dir.display()
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

fn contains(path: &Path, dir: &Path) -> bool {
    if dir.starts_with(path) {
        return true;
    }
    match (std::fs::canonicalize(path), std::fs::canonicalize(dir)) {
        (Ok(path), Ok(dir)) => dir.starts_with(path),
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

pub fn reject_limits(runtime: Runtime, args: &RunArgs) -> Result<(), String> {
    let option = match (args.max_turns, args.max_tokens) {
        (Some(_), _) => "--max-turns",
        (None, Some(_)) => "--max-tokens",
        (None, None) => return Ok(()),
    };
    Err(format!(
        "{option} is not supported by {name}: {name} reports usage only when the run ends, so agentrun cannot stop it during the run",
        name = runtime.name()
    ))
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
    pub images: Vec<Image>,
    pub format: Format,
    pub sandbox: Sandbox,
    pub tempdir: PathBuf,
    pub session: Session,
    pub allow_hosts: Vec<HostRule>,
    pub denied_reads: Vec<PathBuf>,
    pub proxy: Option<ProxyEndpoint>,
    pub resume: Option<Saved>,
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
            images: Vec::new(),
            format: Format::Jsonl,
            sandbox: Sandbox::new(SandboxMode::Off, Wrapper::None),
            tempdir: PathBuf::new(),
            session,
            allow_hosts: Vec::new(),
            denied_reads: Vec::new(),
            proxy: None,
            resume: None,
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
            (Some("anthropic/m"), Some("api.anthropic.com")),
            (Some("openai/m"), Some("api.openai.com")),
            (Some("google/m"), Some("generativelanguage.googleapis.com")),
            (Some("openrouter/m"), Some("openrouter.ai")),
            (Some("acme/robot"), None),
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

        let dsh = |base_url: Option<&str>| {
            let env: Vec<(OsString, OsString)> = base_url
                .map(|url| (OsString::from("DEEPSEEK_BASE_URL"), OsString::from(url)))
                .into_iter()
                .collect();
            Session::assemble(Runtime::Dsh, &env, &[], &[], &[])
        };
        let cases = [
            (None, Some("api.deepseek.com")),
            (
                Some("https://user@Gateway.example:8443/v1"),
                Some("gateway.example"),
            ),
            (Some("gateway"), None),
        ];
        for (base_url, host) in cases {
            assert_eq!(
                service_hosts(Runtime::Dsh, None, &dsh(base_url), cwd),
                host.map(str::to_string).into_iter().collect::<Vec<_>>(),
                "{base_url:?}"
            );
        }
    }

    #[test]
    fn proxy_runs_only_with_a_sandbox_and_depends_on_the_runtime() {
        for mode in [NetworkMode::None, NetworkMode::Full, NetworkMode::Custom] {
            assert!(proxy_needed(Runtime::Pi, mode, true), "{mode:?}");
            assert!(!proxy_needed(Runtime::Pi, mode, false), "{mode:?}");
            assert!(!proxy_needed(Runtime::ClaudeCode, mode, false), "{mode:?}");
            assert!(!proxy_needed(Runtime::Codex, mode, false), "{mode:?}");
        }
        assert!(!proxy_needed(Runtime::ClaudeCode, NetworkMode::None, true));
        assert!(proxy_needed(Runtime::ClaudeCode, NetworkMode::Full, true));
        assert!(proxy_needed(Runtime::ClaudeCode, NetworkMode::Custom, true));
        assert!(!proxy_needed(Runtime::Codex, NetworkMode::None, true));
        assert!(!proxy_needed(Runtime::Codex, NetworkMode::Full, true));
        assert!(proxy_needed(Runtime::Codex, NetworkMode::Custom, true));
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
    fn default_denied_reads_list_home_entries_login_files_but_pis_own_and_histories() {
        let cwd = Path::new("/nonexistent/work");
        let home = [("HOME", "/nonexistent/home")];
        let logins = [
            (
                Runtime::ClaudeCode,
                "/nonexistent/home/.claude/.credentials.json",
            ),
            (Runtime::Codex, "/nonexistent/home/.codex/auth.json"),
            (Runtime::Pi, "/nonexistent/home/.pi/agent/auth.json"),
            (Runtime::Dsh, "/nonexistent/home/.dsh/.env"),
        ];
        for (runtime, _) in logins {
            let mut expected = home_entries("/nonexistent/home");
            expected.extend(
                logins
                    .iter()
                    .filter(|(other, _)| runtime != Runtime::Pi || *other != Runtime::Pi)
                    .map(|(_, login)| PathBuf::from(login)),
            );
            expected.extend(
                [
                    "/nonexistent/home/.claude/projects",
                    "/nonexistent/home/.codex/sessions",
                    "/nonexistent/home/.codex/history.jsonl",
                    "/nonexistent/home/.dsh/sessions",
                ]
                .map(PathBuf::from),
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
                ("DSH_HOME", "/nonexistent/dsh"),
            ],
        );
        let paths = default_denied_reads(Runtime::Pi, &moved, cwd);
        assert_eq!(
            paths[DENIED_HOME_ENTRIES.len()..],
            [
                PathBuf::from("/nonexistent/claude/.credentials.json"),
                PathBuf::from("/nonexistent/work/relative/codex/auth.json"),
                PathBuf::from("/nonexistent/dsh/.env"),
                PathBuf::from("/nonexistent/claude/projects"),
                PathBuf::from("/nonexistent/work/relative/codex/sessions"),
                PathBuf::from("/nonexistent/work/relative/codex/history.jsonl"),
                PathBuf::from("/nonexistent/dsh/sessions"),
            ]
        );
        assert_eq!(
            default_denied_reads(Runtime::Codex, &env_session(Runtime::Codex, &[]), cwd),
            Vec::<PathBuf>::new()
        );
        let dsh = env_session(
            Runtime::Dsh,
            &[
                ("HOME", "/nonexistent/home"),
                ("DSH_HOME", "/nonexistent/dsh"),
            ],
        );
        assert!(!dsh.env.contains_key(OsStr::new("DSH_HOME")));
        let paths = default_denied_reads(Runtime::Dsh, &dsh, cwd);
        for path in ["/nonexistent/dsh/.env", "/nonexistent/dsh/sessions"] {
            assert!(paths.contains(&PathBuf::from(path)), "{path}");
        }
        assert!(!paths.contains(&PathBuf::from("/nonexistent/home/.dsh/.env")));
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
        let temp = Path::new("/nonexistent/tmp");
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
        let paths = denied_reads(Runtime::Pi, &given, &session, &cwd, temp).unwrap();
        assert_eq!(
            paths.len(),
            default_denied_reads(Runtime::Pi, &session, &cwd).len() + 1
        );
        assert_eq!(paths.last(), Some(&cwd.join("secrets")));
        let only = parse(&["--no-default-deny-read", "--deny-read", "/nonexistent/x"]);
        assert_eq!(
            denied_reads(Runtime::Pi, &only, &session, &cwd, temp).unwrap(),
            [PathBuf::from("/nonexistent/x")]
        );
        let parent = cwd.parent().unwrap().to_str().unwrap().to_string();
        let containing = parse(&["--deny-read", &parent]);
        assert_eq!(
            denied_reads(Runtime::Pi, &containing, &session, &cwd, temp),
            Err(format!(
                "--deny-read {parent}: contains the working directory {}",
                cwd.display()
            ))
        );
        let rejected = [
            (
                "/nonexistent",
                "contains the temporary directory /nonexistent/tmp",
            ),
            (
                "/nonexistent/home/.pi",
                "contains the pi state directory /nonexistent/home/.pi/agent",
            ),
            ("~/.ssh", "~ is not expanded, give the full path"),
        ];
        for (given, error) in rejected {
            assert_eq!(
                denied_reads(
                    Runtime::Pi,
                    &parse(&["--deny-read", given]),
                    &session,
                    &cwd,
                    temp
                ),
                Err(format!("--deny-read {given}: {error}"))
            );
        }
        let codex_session = env_session(Runtime::Codex, &[("HOME", "/nonexistent/home")]);
        assert!(
            denied_reads(
                Runtime::Codex,
                &parse(&["--deny-read", "/nonexistent/home/.pi"]),
                &codex_session,
                &cwd,
                temp
            )
            .is_ok()
        );
    }

    #[test]
    fn version_is_found_in_each_runtime_version_output() {
        let cases = [
            ("2.1.303 (Claude Code)", Some(Version(2, 1, 303))),
            ("codex-cli 0.159.2", Some(Version(0, 159, 2))),
            ("1.2.0", Some(Version(1, 2, 0))),
            ("v22.10.0", Some(Version(22, 10, 0))),
            ("codex-cli 0.160.0-alpha.3", Some(Version(0, 160, 0))),
            ("version 1.2", None),
            ("1.x.3", None),
            ("+1.2.3", None),
            ("", None),
        ];
        for (text, version) in cases {
            assert_eq!(Version::find(text), version, "{text}");
        }
        assert!(Version(2, 1, 287) < Version(2, 1, 288));
        assert!(Version(2, 2, 0) > Version(2, 1, 288));
        assert!(Version(0, 160, 0) > Version(0, 159, 2));
        assert_eq!(Version(0, 159, 2).to_string(), "0.159.2");
    }

    #[test]
    fn min_versions_match_the_documented_tables() {
        let run_page = include_str!("../../docs/pages/run.html");
        let quickstart = include_str!("../../docs/pages/quickstart.html");
        for runtime in [
            Runtime::ClaudeCode,
            Runtime::Codex,
            Runtime::Pi,
            Runtime::Dsh,
        ] {
            let run_item = format!("<li>{}: >= ", runtime.name());
            let documented = run_page
                .lines()
                .find_map(|line| line.trim().strip_prefix(run_item.as_str()))
                .and_then(Version::find);
            assert_eq!(
                documented,
                Some(min_version(runtime)),
                "run.html {runtime:?}"
            );
            let quickstart_row = format!("<code>agentrun {}</code>", runtime.name());
            let documented = quickstart
                .lines()
                .find(|line| line.contains(&quickstart_row))
                .and_then(|line| line.split(&quickstart_row).nth(1))
                .and_then(Version::find);
            assert_eq!(
                documented,
                Some(min_version(runtime)),
                "quickstart.html {runtime:?}"
            );
        }
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
