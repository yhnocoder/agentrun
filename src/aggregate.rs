use std::collections::HashMap;
use std::time::Instant;

use crate::adapter::Record;
use crate::event::{self, Body, Event, SubagentStatus};
use crate::usage::{TokenCounts, Usage};

const SUMMARY_MAX_CHARS: usize = 120;

pub struct Aggregator {
    echoes_prompt: bool,
    open_tools: Vec<OpenTool>,
    subagent_parents: HashMap<String, Option<String>>,
    running: Vec<RunningSubagent>,
    started_subagents: u64,
    usages: Vec<UsageEntry>,
    run_usage: Option<Usage>,
    result: Option<String>,
    last_main_text: Option<String>,
}

struct OpenTool {
    id: String,
    parent: Option<String>,
    name: String,
    summary: String,
}

struct RunningSubagent {
    id: String,
    started: Instant,
}

struct UsageEntry {
    parent: Option<String>,
    model: Option<String>,
    counts: TokenCounts,
}

pub struct Summary {
    pub usage: Usage,
    pub result: Option<String>,
}

impl Aggregator {
    pub fn new(echoes_prompt: bool) -> Aggregator {
        Aggregator {
            echoes_prompt,
            open_tools: Vec::new(),
            subagent_parents: HashMap::new(),
            running: Vec::new(),
            started_subagents: 0,
            usages: Vec::new(),
            run_usage: None,
            result: None,
            last_main_text: None,
        }
    }

    pub fn begin(&self, prompt: &str) -> Vec<Event> {
        if self.echoes_prompt {
            Vec::new()
        } else {
            vec![prompt_event(prompt)]
        }
    }

    pub fn push(&mut self, record: Record) -> Vec<Event> {
        match record {
            Record::PromptEcho { text } => {
                if self.echoes_prompt {
                    vec![prompt_event(&text)]
                } else {
                    Vec::new()
                }
            }
            Record::ToolStart {
                id,
                parent,
                name,
                summary,
            } => {
                self.open_tools.push(OpenTool {
                    id,
                    parent,
                    name,
                    summary,
                });
                Vec::new()
            }
            Record::ToolEnd { id, denied } => {
                match self.open_tools.iter().position(|tool| tool.id == id) {
                    Some(index) => {
                        let tool = self.open_tools.remove(index);
                        vec![self.tool_event(tool, denied)]
                    }
                    None => Vec::new(),
                }
            }
            Record::SubagentStart {
                id,
                parent,
                kind,
                model,
                description,
            } => {
                let parent = self.known_parent(parent);
                self.started_subagents += 1;
                self.subagent_parents.insert(id.clone(), parent.clone());
                self.running.push(RunningSubagent {
                    id: id.clone(),
                    started: Instant::now(),
                });
                vec![Event::now(Body::SubagentStart(event::SubagentStart {
                    id,
                    parent,
                    number: self.started_subagents,
                    kind,
                    model,
                    description,
                }))]
            }
            Record::SubagentEnd { id, status } => {
                match self.running.iter().position(|subagent| subagent.id == id) {
                    Some(index) => {
                        let subagent = self.running.remove(index);
                        vec![self.subagent_end_event(subagent, status)]
                    }
                    None => Vec::new(),
                }
            }
            Record::Text { parent, text } => {
                let parent = self.known_parent(parent);
                if parent.is_none() {
                    self.last_main_text = Some(text.clone());
                }
                vec![Event::now(Body::Text(event::Text { parent, text }))]
            }
            Record::Usage {
                parent,
                model,
                counts,
            } => {
                let parent = self.known_parent(parent);
                self.usages.push(UsageEntry {
                    parent: parent.clone(),
                    model: model.clone(),
                    counts,
                });
                vec![Event::now(Body::Usage(event::UsageReport {
                    parent,
                    model,
                    counts,
                    context_tokens: counts.context_tokens(),
                }))]
            }
            Record::RunUsage(usage) => {
                self.run_usage = Some(usage);
                Vec::new()
            }
            Record::Result { text } => {
                self.result = Some(text);
                Vec::new()
            }
            Record::Debug(_) | Record::Terminate => Vec::new(),
        }
    }

    pub fn finish(mut self) -> (Vec<Event>, Summary) {
        let mut events = Vec::new();
        for tool in std::mem::take(&mut self.open_tools) {
            events.push(self.tool_event(tool, false));
        }
        for subagent in std::mem::take(&mut self.running) {
            events.push(self.subagent_end_event(subagent, SubagentStatus::Failed));
        }
        let usage = match self.run_usage.take() {
            Some(usage) => usage,
            None => Usage::sum(
                self.usages
                    .iter()
                    .map(|entry| (entry.model.as_deref(), &entry.counts)),
            ),
        };
        let result = self.result.take().or(self.last_main_text.take());
        (events, Summary { usage, result })
    }

    fn known_parent(&self, parent: Option<String>) -> Option<String> {
        parent.filter(|id| self.subagent_parents.contains_key(id))
    }

    fn tool_event(&self, tool: OpenTool, denied: bool) -> Event {
        Event::now(Body::Tool(event::Tool {
            id: tool.id,
            parent: self.known_parent(tool.parent),
            name: tool.name,
            summary: truncate_summary(tool.summary),
            denied,
        }))
    }

    fn subagent_end_event(&self, subagent: RunningSubagent, status: SubagentStatus) -> Event {
        let usage = Usage::sum(
            self.usages
                .iter()
                .filter(|entry| self.within(entry.parent.as_deref(), &subagent.id))
                .map(|entry| (entry.model.as_deref(), &entry.counts)),
        );
        Event::now(Body::SubagentEnd(event::SubagentEnd {
            id: subagent.id,
            status,
            duration_ms: subagent.started.elapsed().as_millis() as u64,
            usage,
        }))
    }

    fn within<'a>(&'a self, mut parent: Option<&'a str>, subagent: &str) -> bool {
        while let Some(id) = parent {
            if id == subagent {
                return true;
            }
            parent = self
                .subagent_parents
                .get(id)
                .and_then(|next| next.as_deref());
        }
        false
    }
}

fn prompt_event(text: &str) -> Event {
    Event::now(Body::Prompt(event::Prompt {
        text: text.to_string(),
    }))
}

fn truncate_summary(summary: String) -> String {
    if summary.chars().count() <= SUMMARY_MAX_CHARS {
        return summary;
    }
    let mut truncated: String = summary.chars().take(SUMMARY_MAX_CHARS - 1).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_longer_than_limit_is_truncated_with_ellipsis() {
        let long = "a".repeat(130);
        let truncated = truncate_summary(long);
        assert_eq!(truncated.chars().count(), SUMMARY_MAX_CHARS);
        assert!(truncated.ends_with('…'));
        assert_eq!(truncate_summary("b".repeat(120)), "b".repeat(120));
    }
}
