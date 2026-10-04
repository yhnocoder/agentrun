use std::ffi::OsString;
use std::path::Path;

use serde_json::Value;

use crate::claudecode::ClaudeCode;
use crate::cli::Runtime;
use crate::event::SubagentStatus;
use crate::pi::Pi;
use crate::run::Invocation;
use crate::usage::{TokenCounts, Usage};

pub trait Adapter {
    fn runtime(&self) -> Runtime;
    fn launch(&mut self, executable: &Path, invocation: &Invocation) -> Result<Launch, String>;
    fn echoes_prompt(&self) -> bool;
    fn translate(&mut self, line: &Value) -> Vec<Record>;
    fn after_exit(&mut self) -> Vec<Record>;
    fn failure(&self, exit_code: Option<i32>, stderr_tail: &str) -> Option<String>;
}

pub fn builtin(runtime: Runtime) -> Option<Box<dyn Adapter>> {
    match runtime {
        Runtime::ClaudeCode => Some(Box::new(ClaudeCode::new())),
        Runtime::Pi => Some(Box::new(Pi::new())),
        Runtime::Codex => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub argv: Vec<String>,
    pub stdin: Vec<u8>,
    pub env: Vec<(OsString, OsString)>,
    pub signal_wrapped_child: bool,
    pub service_hosts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Record {
    PromptEcho {
        text: String,
    },
    ToolStart {
        id: String,
        parent: Option<String>,
        name: String,
        summary: String,
    },
    ToolEnd {
        id: String,
        denied: bool,
    },
    SubagentStart {
        id: String,
        parent: Option<String>,
        kind: String,
        model: Option<String>,
        description: String,
    },
    SubagentEnd {
        id: String,
        status: SubagentStatus,
    },
    Text {
        parent: Option<String>,
        text: String,
    },
    Usage {
        parent: Option<String>,
        model: Option<String>,
        counts: TokenCounts,
    },
    RunUsage(Usage),
    Result {
        text: String,
    },
    Debug(String),
    Terminate,
}
