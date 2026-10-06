use std::collections::HashMap;
use std::io::Write;

use crate::cli::{Format, SandboxMode};
use crate::event::{Body, EndStatus, Event, SandboxKind};
use crate::rich::{OpenTool, Rich};
use crate::usage::TokenCounts;

pub enum Output {
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

pub struct TextFormatter {
    sandbox: SandboxMode,
    sandbox_reason: String,
    labels: HashMap<String, String>,
    subagent_models: HashMap<String, String>,
}

impl TextFormatter {
    pub fn new(sandbox: SandboxMode, sandbox_reason: &str) -> TextFormatter {
        TextFormatter {
            sandbox,
            sandbox_reason: sandbox_reason.to_string(),
            labels: HashMap::new(),
            subagent_models: HashMap::new(),
        }
    }

    pub fn lines(&mut self, event: &Event) -> Vec<String> {
        match &event.body {
            Body::Start(start) => self.note(start.sandbox).into_iter().collect(),
            Body::Prompt(prompt) => vec![format!("[main] prompt {}", first_line(&prompt.text))],
            Body::Tool(tool) => {
                let denied = if tool.denied { " (denied)" } else { "" };
                vec![format!(
                    "[{}] tool {}{}",
                    self.label(tool.parent.as_deref()),
                    first_line(&tool.summary),
                    denied
                )]
            }
            Body::Text(text) => vec![format!(
                "[{}] text {}",
                self.label(text.parent.as_deref()),
                first_line(&text.text)
            )],
            Body::Usage(usage) => {
                if let (Some(parent), Some(model)) = (&usage.parent, &usage.model) {
                    self.subagent_models.insert(parent.clone(), model.clone());
                }
                Vec::new()
            }
            Body::SubagentStart(start) => {
                let label = subagent_label(&start.kind, start.number);
                self.labels.insert(start.id.clone(), label.clone());
                if let Some(model) = &start.model {
                    self.subagent_models
                        .entry(start.id.clone())
                        .or_insert_with(|| model.clone());
                }
                let model = match &start.model {
                    Some(model) => format!(" ({model})"),
                    None => String::new(),
                };
                vec![format!(
                    "[{}] agent start {}{}: {}",
                    self.label(start.parent.as_deref()),
                    label,
                    model,
                    first_line(&start.description)
                )]
            }
            Body::SubagentEnd(end) => {
                let mut line = format!(
                    "[{}] agent {} {}",
                    self.label(Some(&end.id)),
                    end.status.name(),
                    seconds(end.duration_ms)
                );
                if let Some(model) = self.subagent_models.get(&end.id) {
                    line.push(' ');
                    line.push_str(model);
                }
                line.push_str(&usage_items(&end.usage.totals));
                vec![line]
            }
            Body::Network(network) => match network.reason {
                Some(reason) if !network.allowed => vec![format!(
                    "[main] net denied {}:{} ({})",
                    network.host,
                    network.port,
                    reason.name()
                )],
                _ => Vec::new(),
            },
            Body::End(end) => {
                if end.status == EndStatus::Rejected {
                    return vec![format!("[end] rejected {}", single_line(&end.detail))];
                }
                let mut line = format!("[end] {} {}", end.status.name(), seconds(end.duration_ms));
                line.push_str(&usage_items(&end.usage.totals));
                if !end.detail.is_empty() {
                    line.push_str("  ");
                    line.push_str(&single_line(&end.detail));
                }
                vec![line]
            }
        }
    }

    fn note(&self, sandbox: SandboxKind) -> Option<String> {
        if sandbox != SandboxKind::None {
            return None;
        }
        let mode = match self.sandbox {
            SandboxMode::On => return None,
            SandboxMode::Relax => format!("--sandbox relax: {}", self.sandbox_reason),
            SandboxMode::Off => "--sandbox off".to_string(),
        };
        Some(format!(
            "[note] sandbox not running ({mode}). agentrun does not restrict what the agent writes or which hosts it reaches"
        ))
    }

    fn label(&self, agent: Option<&str>) -> String {
        agent
            .and_then(|id| self.labels.get(id))
            .cloned()
            .unwrap_or_else(|| "main".to_string())
    }
}

pub fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn single_line(text: &str) -> String {
    text.replace("\r\n", " ").replace(['\n', '\r'], " ")
}

pub fn subagent_label(kind: &str, number: u64) -> String {
    format!("{}#{}", kind.to_lowercase(), number)
}

fn seconds(duration_ms: u64) -> String {
    let tenths = (duration_ms + 50) / 100;
    format!("{}.{}s", tenths / 10, tenths % 10)
}

fn usage_items(counts: &TokenCounts) -> String {
    counts
        .summary(true)
        .iter()
        .map(|item| format!("  {item}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{NetworkMode, Runtime};
    use crate::event::{
        End, Network, NetworkInfo, NetworkReason, Prompt, Start, SubagentEnd, SubagentStart,
        SubagentStatus, Text, Tool, UsageReport,
    };
    use crate::usage::Usage;

    fn event(body: Body) -> Event {
        Event {
            time: "2026-10-04T13:00:00.000Z".to_string(),
            body,
        }
    }

    fn start() -> Event {
        event(Body::Start(Start {
            runtime: Runtime::ClaudeCode,
            sandbox: SandboxKind::None,
            network: NetworkInfo {
                mode: NetworkMode::None,
                allow: vec![],
                enforced: false,
            },
            model: None,
            cwd: "/work".to_string(),
            argv: vec![],
            env: vec![],
        }))
    }

    fn counts(
        input: Option<u64>,
        output: Option<u64>,
        read: Option<u64>,
        write: Option<u64>,
    ) -> TokenCounts {
        TokenCounts {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: read,
            cache_write_tokens: write,
        }
    }

    fn end(status: EndStatus, detail: &str, totals: TokenCounts) -> Event {
        event(Body::End(End {
            status,
            exit_code: Some(0),
            detail: detail.to_string(),
            duration_ms: 31_250,
            usage: Usage {
                totals,
                by_model: Default::default(),
            },
            result: None,
        }))
    }

    #[test]
    fn note_line_for_relax_and_off() {
        let mut relax = TextFormatter::new(SandboxMode::Relax, "not implemented in this build");
        assert_eq!(
            relax.lines(&start()),
            vec![
                "[note] sandbox not running (--sandbox relax: not implemented in this build). agentrun does not restrict what the agent writes or which hosts it reaches"
            ]
        );
        let mut off = TextFormatter::new(SandboxMode::Off, "not implemented in this build");
        assert_eq!(
            off.lines(&start()),
            vec![
                "[note] sandbox not running (--sandbox off). agentrun does not restrict what the agent writes or which hosts it reaches"
            ]
        );
        let mut on = TextFormatter::new(SandboxMode::On, "");
        assert!(on.lines(&start()).is_empty());
    }

    #[test]
    fn prompt_text_and_tool_lines() {
        let mut text = TextFormatter::new(SandboxMode::On, "");
        assert_eq!(
            text.lines(&event(Body::Prompt(Prompt {
                text: "fix tests\nmore".to_string()
            }))),
            vec!["[main] prompt fix tests"]
        );
        assert_eq!(
            text.lines(&event(Body::Text(Text {
                parent: None,
                text: "done\nsecond".to_string()
            }))),
            vec!["[main] text done"]
        );
        assert_eq!(
            text.lines(&event(Body::Tool(Tool {
                id: "t1".to_string(),
                parent: None,
                name: "Bash".to_string(),
                summary: "Bash: cat ~/.ssh/id_rsa".to_string(),
                denied: true,
            }))),
            vec!["[main] tool Bash: cat ~/.ssh/id_rsa (denied)"]
        );
    }

    #[test]
    fn subagent_lines_use_labels_and_latest_model() {
        let mut text = TextFormatter::new(SandboxMode::On, "");
        assert_eq!(
            text.lines(&event(Body::SubagentStart(SubagentStart {
                id: "s1".to_string(),
                parent: None,
                number: 1,
                kind: "Explore".to_string(),
                model: Some("haiku".to_string()),
                description: "find callers".to_string(),
            }))),
            vec!["[main] agent start explore#1 (haiku): find callers"]
        );
        assert_eq!(
            text.lines(&event(Body::SubagentStart(SubagentStart {
                id: "s2".to_string(),
                parent: Some("s1".to_string()),
                number: 2,
                kind: "general-purpose".to_string(),
                model: None,
                description: "write tests".to_string(),
            }))),
            vec!["[explore#1] agent start general-purpose#2: write tests"]
        );
        assert!(
            text.lines(&event(Body::Usage(UsageReport {
                parent: Some("s1".to_string()),
                model: Some("claude-haiku-4-5".to_string()),
                counts: counts(Some(3120), Some(85), None, None),
                context_tokens: None,
            })))
            .is_empty()
        );
        assert_eq!(
            text.lines(&event(Body::Tool(Tool {
                id: "t2".to_string(),
                parent: Some("s2".to_string()),
                name: "Read".to_string(),
                summary: "Read: tests/parse.rs".to_string(),
                denied: false,
            }))),
            vec!["[general-purpose#2] tool Read: tests/parse.rs"]
        );
        assert_eq!(
            text.lines(&event(Body::SubagentEnd(SubagentEnd {
                id: "s1".to_string(),
                status: SubagentStatus::Finished,
                duration_ms: 12_400,
                usage: Usage {
                    totals: counts(Some(3120), Some(85), Some(1), Some(2)),
                    by_model: Default::default(),
                },
            }))),
            vec!["[explore#1] agent finished 12.4s claude-haiku-4-5  in 3.1k  out 85  cached 0%"]
        );
        assert_eq!(
            text.lines(&event(Body::SubagentEnd(SubagentEnd {
                id: "s2".to_string(),
                status: SubagentStatus::Failed,
                duration_ms: 50,
                usage: Usage::default(),
            }))),
            vec!["[general-purpose#2] agent failed 0.1s"]
        );
    }

    #[test]
    fn network_lines_show_only_denied() {
        let mut text = TextFormatter::new(SandboxMode::On, "");
        assert_eq!(
            text.lines(&event(Body::Network(Network {
                host: "evil.example".to_string(),
                port: 443,
                allowed: false,
                reason: Some(NetworkReason::NotAllowed),
            }))),
            vec!["[main] net denied evil.example:443 (not_allowed)"]
        );
        assert!(
            text.lines(&event(Body::Network(Network {
                host: "pypi.org".to_string(),
                port: 443,
                allowed: true,
                reason: None,
            })))
            .is_empty()
        );
    }

    #[test]
    fn end_lines() {
        let mut text = TextFormatter::new(SandboxMode::On, "");
        assert_eq!(
            text.lines(&end(
                EndStatus::Finished,
                "",
                counts(Some(18234), Some(912), Some(42000), Some(5100))
            )),
            vec!["[end] finished 31.3s  in 65.3k  out 0.9k  cached 64%"]
        );
        assert_eq!(
            text.lines(&end(
                EndStatus::Failed,
                "boom",
                counts(None, Some(3), None, Some(4))
            )),
            vec!["[end] failed 31.3s  in 4  out 3  boom"]
        );
        assert_eq!(
            text.lines(&end(EndStatus::Failed, "", TokenCounts::default())),
            vec!["[end] failed 31.3s"]
        );
        assert_eq!(
            text.lines(&end(
                EndStatus::Rejected,
                "claude not found in PATH",
                TokenCounts::default()
            )),
            vec!["[end] rejected claude not found in PATH"]
        );
    }

    #[test]
    fn end_lines_put_a_multiline_detail_on_one_line() {
        let mut text = TextFormatter::new(SandboxMode::On, "");
        assert_eq!(
            text.lines(&end(
                EndStatus::Failed,
                "first\nsecond\r\nthird\rfourth",
                TokenCounts::default()
            )),
            vec!["[end] failed 31.3s  first second third fourth"]
        );
        assert_eq!(
            text.lines(&end(
                EndStatus::Rejected,
                "cannot start\r\nsee above\n",
                TokenCounts::default()
            )),
            vec!["[end] rejected cannot start see above "]
        );
    }
}
