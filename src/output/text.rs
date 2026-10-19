use std::collections::HashMap;

use super::event::{Body, EndStatus, Event, SandboxKind};
use super::usage::{TokenCounts, cost};
use crate::cli::SandboxMode;
use crate::json::first_line;

pub struct TextFormatter {
    sandbox: SandboxMode,
    sandbox_reason: String,
    notes: Vec<String>,
    labels: HashMap<String, String>,
    subagent_models: HashMap<String, String>,
}

impl TextFormatter {
    pub fn new(sandbox: SandboxMode, sandbox_reason: &str) -> TextFormatter {
        TextFormatter {
            sandbox,
            sandbox_reason: sandbox_reason.to_string(),
            notes: Vec::new(),
            labels: HashMap::new(),
            subagent_models: HashMap::new(),
        }
    }

    pub fn lines(&mut self, event: &Event) -> Vec<String> {
        match &event.body {
            Body::Start(start) => self
                .note(start.sandbox)
                .into_iter()
                .chain(self.notes.iter().map(|note| format!("[note] {note}")))
                .collect(),
            Body::Prompt(prompt) => vec![format!(
                "[main] prompt {}{}",
                first_line(&prompt.text),
                image_count(prompt.images.len())
            )],
            Body::ToolStart(_) => Vec::new(),
            Body::Tool(tool) => {
                let outcome = if tool.denied {
                    " (denied)"
                } else if tool.error == Some(true) {
                    " (error)"
                } else {
                    ""
                };
                vec![format!(
                    "[{}] tool {}{}",
                    self.label(tool.parent.as_deref()),
                    first_line(&tool.summary),
                    outcome
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
                let label = label_for(&start.kind, start.number);
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
                if let Some(usd) = end.usage.totals.cost_usd {
                    line.push_str("  ");
                    line.push_str(&cost(usd));
                }
                if let Some(id) = &end.session_id {
                    line.push_str("  session ");
                    line.push_str(id);
                }
                if !end.detail.is_empty() {
                    line.push_str("  ");
                    line.push_str(&single_line(&end.detail));
                }
                vec![line]
            }
        }
    }

    pub fn add_note(&mut self, note: String) {
        self.notes.push(note);
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
            "[note] sandbox not running ({mode}). agentrun does not restrict what the agent reads or writes or which hosts it reaches"
        ))
    }

    pub fn subagent_label(&self, id: &str) -> Option<&str> {
        self.labels.get(id).map(String::as_str)
    }

    pub fn subagent_model(&self, id: &str) -> Option<&str> {
        self.subagent_models.get(id).map(String::as_str)
    }

    fn label(&self, agent: Option<&str>) -> String {
        agent
            .and_then(|id| self.labels.get(id))
            .cloned()
            .unwrap_or_else(|| "main".to_string())
    }
}

fn single_line(text: &str) -> String {
    text.trim_end_matches(['\r', '\n'])
        .replace("\r\n", " ")
        .replace(['\n', '\r'], " ")
}

fn label_for(kind: &str, number: u64) -> String {
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

fn image_count(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => " (+1 image)".to_string(),
        count => format!(" (+{count} images)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{NetworkMode, Runtime};
    use crate::output::event::{
        End, Network, NetworkInfo, NetworkReason, Prompt, Start, SubagentEnd, SubagentStart,
        SubagentStatus, Text, Tool, ToolStart, UsageReport,
    };
    use crate::output::usage::Usage;

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
            deny_read: vec![],
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
            cost_usd: None,
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
            session_id: None,
        }))
    }

    #[test]
    fn note_line_for_relax_and_off() {
        let mut relax = TextFormatter::new(SandboxMode::Relax, "not implemented in this build");
        assert_eq!(
            relax.lines(&start()),
            vec![
                "[note] sandbox not running (--sandbox relax: not implemented in this build). agentrun does not restrict what the agent reads or writes or which hosts it reaches"
            ]
        );
        let mut off = TextFormatter::new(SandboxMode::Off, "not implemented in this build");
        assert_eq!(
            off.lines(&start()),
            vec![
                "[note] sandbox not running (--sandbox off). agentrun does not restrict what the agent reads or writes or which hosts it reaches"
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
                text: "fix tests\nmore".to_string(),
                images: Vec::new()
            }))),
            vec!["[main] prompt fix tests"]
        );
        assert_eq!(
            text.lines(&event(Body::Prompt(Prompt {
                text: "read this".to_string(),
                images: vec!["a.png".to_string()]
            }))),
            vec!["[main] prompt read this (+1 image)"]
        );
        assert_eq!(
            text.lines(&event(Body::Prompt(Prompt {
                text: "compare".to_string(),
                images: vec!["a.png".to_string(), "b.png".to_string()]
            }))),
            vec!["[main] prompt compare (+2 images)"]
        );
        assert_eq!(
            text.lines(&event(Body::Text(Text {
                parent: None,
                text: "done\nsecond".to_string()
            }))),
            vec!["[main] text done"]
        );
        assert!(
            text.lines(&event(Body::ToolStart(ToolStart {
                id: "t1".to_string(),
                parent: None,
                name: "Bash".to_string(),
                summary: "Bash: cat ~/.ssh/id_rsa".to_string(),
            })))
            .is_empty()
        );
        assert_eq!(
            text.lines(&event(Body::Tool(Tool {
                id: "t1".to_string(),
                parent: None,
                name: "Bash".to_string(),
                summary: "Bash: cat ~/.ssh/id_rsa".to_string(),
                denied: true,
                error: Some(false),
            }))),
            vec!["[main] tool Bash: cat ~/.ssh/id_rsa (denied)"]
        );
        for (error, line) in [
            (Some(true), "[main] tool Bash: cargo test (error)"),
            (Some(false), "[main] tool Bash: cargo test"),
            (None, "[main] tool Bash: cargo test"),
        ] {
            assert_eq!(
                text.lines(&event(Body::Tool(Tool {
                    id: "t2".to_string(),
                    parent: None,
                    name: "Bash".to_string(),
                    summary: "Bash: cargo test".to_string(),
                    denied: false,
                    error,
                }))),
                vec![line]
            );
        }
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
                error: Some(false),
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
            text.lines(&end(
                EndStatus::Finished,
                "",
                TokenCounts {
                    cost_usd: Some(0.04369),
                    ..counts(Some(18234), Some(912), Some(42000), Some(5100))
                }
            )),
            vec!["[end] finished 31.3s  in 65.3k  out 0.9k  cached 64%  $0.0437"]
        );
        assert_eq!(
            text.lines(&end(
                EndStatus::Failed,
                "boom",
                TokenCounts {
                    cost_usd: Some(0.0),
                    ..TokenCounts::default()
                }
            )),
            vec!["[end] failed 31.3s  $0.0000  boom"]
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
        let mut saved = end(EndStatus::Failed, "boom", counts(None, Some(3), None, None));
        if let Body::End(end) = &mut saved.body {
            end.session_id = Some("0199-a".to_string());
        }
        assert_eq!(
            text.lines(&saved),
            vec!["[end] failed 31.3s  out 3  session 0199-a  boom"]
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
            vec!["[end] rejected cannot start see above"]
        );
    }
}
