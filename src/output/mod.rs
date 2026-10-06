mod aggregate;
mod event;
mod rich;
mod text;
mod usage;

use std::io::Write;

use crate::cli::{Format, SandboxMode};

pub use aggregate::{Aggregator, Record};
pub use event::{Event, Network, NetworkReason, SandboxKind, SubagentStatus};
pub use rich::Rich;
pub use text::TextFormatter;
pub use usage::{TokenCounts, Usage};

pub(crate) use event::{Body, End, EndStatus, NetworkInfo, Signal, Start};
pub(crate) use rich::{OpenTool, REFRESH_PERIOD, terminal_size};

pub(crate) enum Output {
    Jsonl,
    Text(TextFormatter),
    Rich(Box<Rich>),
}

impl Output {
    pub fn new(format: Format, sandbox: SandboxMode, sandbox_reason: &str) -> Output {
        match format {
            Format::Jsonl => Output::Jsonl,
            Format::Text | Format::Rich => {
                Output::Text(TextFormatter::new(sandbox, sandbox_reason))
            }
        }
    }

    pub fn write(&mut self, out: &mut dyn Write, event: &Event, open_tool: OpenTool) {
        let lines = match self {
            Output::Jsonl => vec![event.to_json()],
            Output::Text(formatter) => formatter.lines(event),
            Output::Rich(rich) => return rich.event(out, event, open_tool),
        };
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
    }

    pub fn refresh(&mut self, out: &mut dyn Write, open_tool: OpenTool) {
        if let Output::Rich(rich) = self {
            rich.refresh(out, open_tool);
        }
    }

    pub fn stderr(&mut self, out: &mut dyn Write, bytes: &[u8], open_tool: OpenTool) {
        if let Output::Rich(rich) = self {
            rich.stderr(out, bytes, open_tool);
        }
    }

    pub fn is_rich(&self) -> bool {
        matches!(self, Output::Rich(_))
    }
}
