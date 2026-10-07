pub mod claudecode;
pub mod codex;
pub mod pi;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cli::{Format, RunArgs, Runtime};
use crate::network::{HostRule, ProxyEndpoint};
use crate::output::{Record, SandboxKind};
use crate::sandbox::{PiState, Sandbox};
use crate::session::Session;
use claudecode::ClaudeCode;
use codex::Codex;
use pi::Pi;

pub const DETAIL_MAX_CHARS: usize = 500;

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
    pub proxy: Option<ProxyEndpoint>,
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
