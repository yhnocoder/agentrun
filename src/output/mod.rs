mod aggregate;
mod event;
mod rich;
mod text;
mod usage;

use std::io::Write;

use crate::cli::{Format, SandboxMode};

pub use aggregate::{Aggregator, OpenTools, Record, Translated};
pub use event::{Event, SandboxKind, SubagentStatus};
pub use rich::Rich;
pub use text::TextFormatter;
pub use usage::{TokenCounts, Usage};

pub(crate) use event::{Body, End, EndStatus, Network, NetworkInfo, NetworkReason, Signal, Start};
pub(crate) use rich::REFRESH_PERIOD;

pub(crate) enum Output {
    Jsonl,
    Text(TextFormatter),
    Rich(Box<Rich>),
}

impl Output {
    pub fn select(
        format: Format,
        stdout_is_terminal: bool,
        color: bool,
        sandbox: SandboxMode,
        sandbox_reason: &str,
    ) -> Output {
        match format {
            Format::Rich if stdout_is_terminal => Output::Rich(Box::new(Rich::new(
                sandbox,
                sandbox_reason,
                color,
                Box::new(rich::terminal_size),
            ))),
            Format::Jsonl => Output::Jsonl,
            Format::Text | Format::Rich => {
                Output::Text(TextFormatter::new(sandbox, sandbox_reason))
            }
        }
    }

    pub fn set_open_tools(&mut self, open_tools: OpenTools) {
        if let Output::Rich(rich) = self {
            rich.set_open_tools(open_tools);
        }
    }

    pub fn write(&mut self, out: &mut dyn Write, event: &Event) {
        let lines = match self {
            Output::Jsonl => vec![event.to_json()],
            Output::Text(formatter) => formatter.lines(event),
            Output::Rich(rich) => return rich.event(out, event),
        };
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
    }

    pub fn refresh(&mut self, out: &mut dyn Write) {
        if let Output::Rich(rich) = self {
            rich.refresh(out);
        }
    }

    pub fn stderr(&mut self, out: &mut dyn Write, bytes: &[u8]) {
        if let Output::Rich(rich) = self {
            rich.stderr(out, bytes);
        }
    }

    pub fn is_rich(&self) -> bool {
        matches!(self, Output::Rich(_))
    }
}

pub(crate) fn write_end(out: &mut dyn Write, format: Format, end: End) {
    let event = Event::now(Body::End(end));
    let lines = match format {
        Format::Jsonl => vec![event.to_json()],
        Format::Text | Format::Rich => TextFormatter::new(SandboxMode::On, "").lines(&event),
    };
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serde_json::Value;

    use super::*;

    fn written(format: Format) -> String {
        let mut out = Vec::new();
        let end = End::early(
            EndStatus::Rejected,
            "claude not found in PATH".to_string(),
            Instant::now(),
        );
        write_end(&mut out, format, end);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn write_end_writes_one_json_line_or_the_text_end_line() {
        let jsonl = written(Format::Jsonl);
        assert_eq!(jsonl.lines().count(), 1);
        let value: Value = serde_json::from_str(jsonl.trim_end()).unwrap();
        assert_eq!(value["type"], "end");
        assert_eq!(value["status"], "rejected");
        assert_eq!(value["exit_code"], Value::Null);
        assert_eq!(value["detail"], "claude not found in PATH");
        for format in [Format::Text, Format::Rich] {
            assert_eq!(written(format), "[end] rejected claude not found in PATH\n");
        }
    }
}
