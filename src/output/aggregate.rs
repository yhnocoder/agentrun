use std::collections::HashMap;
use std::time::Instant;

use super::event::{self, Body, Event, SubagentStatus};
use super::usage::{TokenCounts, Usage};

const SUMMARY_MAX_CHARS: usize = 120;

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

pub struct Translated {
    pub events: Vec<Event>,
    pub debug: Vec<String>,
    pub terminate: bool,
}

pub struct Aggregator {
    echoes_prompt: bool,
    open_tools: Vec<StartedTool>,
    subagent_parents: HashMap<String, Option<String>>,
    running: Vec<RunningSubagent>,
    started_subagents: u64,
    usages: Vec<UsageEntry>,
    run_usage: Option<Usage>,
    result: Option<String>,
    last_main_text: Option<String>,
}

struct StartedTool {
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

    pub fn push(&mut self, records: Vec<Record>) -> Translated {
        let mut translated = Translated {
            events: Vec::new(),
            debug: Vec::new(),
            terminate: false,
        };
        for record in records {
            match record {
                Record::PromptEcho { text } => {
                    if self.echoes_prompt {
                        translated.events.push(prompt_event(&text));
                    }
                }
                Record::ToolStart {
                    id,
                    parent,
                    name,
                    summary,
                } => {
                    let tool = StartedTool {
                        id,
                        parent: self.known_parent(parent),
                        name,
                        summary: truncate_summary(&summary),
                    };
                    translated
                        .events
                        .push(Event::now(Body::ToolStart(event::ToolStart {
                            id: tool.id.clone(),
                            parent: tool.parent.clone(),
                            name: tool.name.clone(),
                            summary: tool.summary.clone(),
                        })));
                    self.open_tools.push(tool);
                }
                Record::ToolEnd { id, denied } => {
                    if let Some(index) = self.open_tools.iter().position(|tool| tool.id == id) {
                        let tool = self.open_tools.remove(index);
                        translated.events.push(tool_event(tool, denied));
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
                    translated
                        .events
                        .push(Event::now(Body::SubagentStart(event::SubagentStart {
                            id,
                            parent,
                            number: self.started_subagents,
                            kind,
                            model,
                            description,
                        })));
                }
                Record::SubagentEnd { id, status } => {
                    if let Some(index) = self.running.iter().position(|subagent| subagent.id == id)
                    {
                        let subagent = self.running.remove(index);
                        translated
                            .events
                            .push(self.subagent_end_event(subagent, status));
                    }
                }
                Record::Text { parent, text } => {
                    let parent = self.known_parent(parent);
                    if parent.is_none() {
                        self.last_main_text = Some(text.clone());
                    }
                    translated
                        .events
                        .push(Event::now(Body::Text(event::Text { parent, text })));
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
                    translated
                        .events
                        .push(Event::now(Body::Usage(event::UsageReport {
                            parent,
                            model,
                            counts,
                            context_tokens: counts.context_tokens(),
                        })));
                }
                Record::RunUsage(usage) => self.run_usage = Some(usage),
                Record::Result { text } => self.result = Some(text),
                Record::Debug(line) => translated.debug.push(line),
                Record::Terminate => translated.terminate = true,
            }
        }
        translated
    }

    pub fn finish(mut self) -> (Vec<Event>, Summary) {
        let mut events = Vec::new();
        for tool in std::mem::take(&mut self.open_tools) {
            events.push(tool_event(tool, false));
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

fn tool_event(tool: StartedTool, denied: bool) -> Event {
    Event::now(Body::Tool(event::Tool {
        id: tool.id,
        parent: tool.parent,
        name: tool.name,
        summary: tool.summary,
        denied,
    }))
}

fn prompt_event(text: &str) -> Event {
    Event::now(Body::Prompt(event::Prompt {
        text: text.to_string(),
    }))
}

fn truncate_summary(summary: &str) -> String {
    if summary.chars().count() <= SUMMARY_MAX_CHARS {
        return summary.to_string();
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
        let truncated = truncate_summary(&long);
        assert_eq!(truncated.chars().count(), SUMMARY_MAX_CHARS);
        assert!(truncated.ends_with('…'));
        assert_eq!(truncate_summary(&"b".repeat(120)), "b".repeat(120));
    }

    fn tool_start(id: &str, parent: Option<&str>, summary: &str) -> Record {
        Record::ToolStart {
            id: id.to_string(),
            parent: parent.map(str::to_string),
            name: "Bash".to_string(),
            summary: summary.to_string(),
        }
    }

    fn tool_end(id: &str) -> Record {
        Record::ToolEnd {
            id: id.to_string(),
            denied: false,
        }
    }

    fn subagent_start(id: &str) -> Record {
        Record::SubagentStart {
            id: id.to_string(),
            parent: None,
            kind: "Explore".to_string(),
            model: None,
            description: "look".to_string(),
        }
    }

    fn started(event: &Event) -> &event::ToolStart {
        match &event.body {
            Body::ToolStart(start) => start,
            other => panic!("{other:?}"),
        }
    }

    fn ended(event: &Event) -> &event::Tool {
        match &event.body {
            Body::Tool(tool) => tool,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_start_is_written_when_the_call_starts() {
        let mut aggregator = Aggregator::new(false);
        let long = format!("Bash: {}", "x".repeat(200));
        let translated = aggregator.push(vec![tool_start("t1", None, &long)]);
        assert_eq!(translated.events.len(), 1);
        let start = started(&translated.events[0]).clone();
        assert_eq!(start.id, "t1");
        assert_eq!(start.parent, None);
        assert_eq!(start.name, "Bash");
        assert_eq!(start.summary, truncate_summary(&long));

        let translated = aggregator.push(vec![tool_end("t1")]);
        assert_eq!(translated.events.len(), 1);
        let tool = ended(&translated.events[0]);
        assert_eq!(tool.id, start.id);
        assert_eq!(tool.summary, start.summary);
        assert!(aggregator.push(vec![tool_end("t1")]).events.is_empty());
    }

    #[test]
    fn tool_start_and_tool_of_one_call_have_the_same_parent() {
        let mut aggregator = Aggregator::new(false);
        let translated = aggregator.push(vec![
            tool_start("t1", Some("s1"), "Bash: before"),
            subagent_start("s1"),
            tool_start("t2", Some("s1"), "Read: a.rs"),
            tool_start("t3", Some("unknown"), "Bash: unknown parent"),
            tool_end("t1"),
            tool_end("t2"),
            tool_end("t3"),
        ]);
        let events = &translated.events;
        assert_eq!(events.len(), 7);
        let pairs = [(0, 4), (2, 5), (3, 6)];
        let parents = [None, Some("s1".to_string()), None];
        for ((start, end), parent) in pairs.into_iter().zip(parents) {
            assert_eq!(started(&events[start]).parent, parent);
            assert_eq!(ended(&events[end]).parent, parent);
            assert_eq!(started(&events[start]).id, ended(&events[end]).id);
        }
    }

    #[test]
    fn tool_written_at_finish_follows_its_tool_start() {
        let mut aggregator = Aggregator::new(false);
        let translated = aggregator.push(vec![
            subagent_start("s1"),
            tool_start("t1", Some("s1"), "Bash: sleep"),
        ]);
        let start = started(&translated.events[1]).clone();
        let (events, _) = aggregator.finish();
        let tool = ended(&events[0]);
        assert_eq!(tool.id, start.id);
        assert_eq!(tool.parent, start.parent);
        assert_eq!(tool.summary, start.summary);
        assert!(!tool.denied);
    }

    #[test]
    fn call_that_starts_a_subagent_has_no_tool_start() {
        let mut aggregator = Aggregator::new(false);
        let translated = aggregator.push(vec![subagent_start("s1")]);
        assert_eq!(translated.events.len(), 1);
        assert!(matches!(
            &translated.events[0].body,
            Body::SubagentStart(start) if start.id == "s1"
        ));
        let (events, _) = aggregator.finish();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0].body, Body::SubagentEnd(end) if end.id == "s1"));
    }

    #[test]
    fn push_separates_debug_lines_and_terminate_from_events() {
        let mut aggregator = Aggregator::new(false);
        let translated = aggregator.push(vec![
            Record::Debug("init".to_string()),
            Record::Text {
                parent: None,
                text: "hello".to_string(),
            },
            Record::Terminate,
            Record::Debug("more".to_string()),
        ]);
        assert_eq!(translated.debug, ["init", "more"]);
        assert!(translated.terminate);
        assert_eq!(translated.events.len(), 1);
        assert!(matches!(&translated.events[0].body, Body::Text(text) if text.text == "hello"));

        let translated = aggregator.push(vec![Record::Text {
            parent: None,
            text: "again".to_string(),
        }]);
        assert!(translated.debug.is_empty());
        assert!(!translated.terminate);
    }
}
