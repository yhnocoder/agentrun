pub mod claudecode;
pub mod codex;
pub mod pi;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cli::{Format, RunArgs, Runtime};
use crate::network::{HostRule, ProxyEndpoint};
use crate::output::{Record, SandboxKind};
use crate::sandbox::Sandbox;
use crate::session::Session;
use claudecode::ClaudeCode;
use codex::Codex;
use pi::Pi;

pub const DETAIL_MAX_CHARS: usize = 500;

pub trait Adapter {
    fn runtime(&self) -> Runtime;
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
    pub codex_home: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub argv: Vec<String>,
    pub stdin: Vec<u8>,
    pub env: Vec<(OsString, OsString)>,
    pub signal_wrapped_child: bool,
    pub service_hosts: Vec<String>,
    pub wrapped: SandboxKind,
}

#[cfg(test)]
mod tests {
    use super::*;

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
