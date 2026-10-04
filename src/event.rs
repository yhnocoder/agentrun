use std::time::SystemTime;

use serde::Serialize;
use time::OffsetDateTime;
use time::macros::format_description;

use crate::cli::{NetworkMode, Runtime};
use crate::usage::{TokenCounts, Usage};

pub const SCHEMA: u8 = 1;

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub time: String,
    pub body: Body,
}

impl Event {
    pub fn now(body: Body) -> Event {
        Event {
            time: format_time(SystemTime::now()),
            body,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&Envelope {
            schema: SCHEMA,
            kind: self.body.kind(),
            time: &self.time,
            body: &self.body,
        })
        .expect("events serialize")
    }
}

#[derive(Serialize)]
struct Envelope<'a> {
    schema: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    time: &'a str,
    #[serde(flatten)]
    body: &'a Body,
}

pub fn format_time(time: SystemTime) -> String {
    OffsetDateTime::from(time)
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .expect("time formats")
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Body {
    Start(Start),
    Prompt(Prompt),
    Tool(Tool),
    Text(Text),
    Usage(UsageReport),
    SubagentStart(SubagentStart),
    SubagentEnd(SubagentEnd),
    Network(Network),
    End(End),
}

impl Body {
    pub fn kind(&self) -> &'static str {
        match self {
            Body::Start(_) => "start",
            Body::Prompt(_) => "prompt",
            Body::Tool(_) => "tool",
            Body::Text(_) => "text",
            Body::Usage(_) => "usage",
            Body::SubagentStart(_) => "subagent_start",
            Body::SubagentEnd(_) => "subagent_end",
            Body::Network(_) => "network",
            Body::End(_) => "end",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Start {
    pub runtime: Runtime,
    pub sandbox: SandboxKind,
    pub network: NetworkInfo,
    pub model: Option<String>,
    pub cwd: String,
    pub argv: Vec<String>,
    pub env: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxKind {
    Seatbelt,
    Bubblewrap,
    Codex,
    None,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NetworkInfo {
    pub mode: NetworkMode,
    pub allow: Vec<String>,
    pub enforced: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Prompt {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Tool {
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub summary: String,
    pub denied: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Text {
    pub parent: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageReport {
    pub parent: Option<String>,
    pub model: Option<String>,
    #[serde(flatten)]
    pub counts: TokenCounts,
    pub context_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SubagentStart {
    pub id: String,
    pub parent: Option<String>,
    pub number: u64,
    pub kind: String,
    pub model: Option<String>,
    pub description: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentStatus {
    Finished,
    Failed,
}

impl SubagentStatus {
    pub fn name(self) -> &'static str {
        match self {
            SubagentStatus::Finished => "finished",
            SubagentStatus::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SubagentEnd {
    pub id: String,
    pub status: SubagentStatus,
    pub duration_ms: u64,
    pub usage: Usage,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Network {
    pub host: String,
    pub port: u16,
    pub allowed: bool,
    pub reason: Option<NetworkReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkReason {
    NotAllowed,
    PrivateAddress,
}

impl NetworkReason {
    pub fn name(self) -> &'static str {
        match self {
            NetworkReason::NotAllowed => "not_allowed",
            NetworkReason::PrivateAddress => "private_address",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EndStatus {
    Finished,
    Failed,
    Rejected,
}

impl EndStatus {
    pub fn name(self) -> &'static str {
        match self {
            EndStatus::Finished => "finished",
            EndStatus::Failed => "failed",
            EndStatus::Rejected => "rejected",
        }
    }

    pub fn exit_code(self) -> u8 {
        match self {
            EndStatus::Finished => 0,
            EndStatus::Failed => 1,
            EndStatus::Rejected => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct End {
    pub status: EndStatus,
    pub exit_code: Option<i32>,
    pub detail: String,
    pub duration_ms: u64,
    pub usage: Usage,
    pub result: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    fn at(body: Body) -> Event {
        Event {
            time: "2026-10-04T13:00:00.000Z".to_string(),
            body,
        }
    }

    #[test]
    fn time_is_utc_with_milliseconds() {
        let time = UNIX_EPOCH + Duration::from_millis(1_791_118_800_007);
        assert_eq!(format_time(time), "2026-10-04T13:00:00.007Z");
        assert_eq!(format_time(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn start_fields_in_order() {
        let event = at(Body::Start(Start {
            runtime: Runtime::ClaudeCode,
            sandbox: SandboxKind::None,
            network: NetworkInfo {
                mode: NetworkMode::Custom,
                allow: vec!["pypi.org".to_string()],
                enforced: false,
            },
            model: None,
            cwd: "/work".to_string(),
            argv: vec!["/bin/claude".to_string(), "-p".to_string()],
            env: vec![],
        }));
        assert_eq!(
            event.to_json(),
            r#"{"schema":1,"type":"start","time":"2026-10-04T13:00:00.000Z","runtime":"claude-code","sandbox":"none","network":{"mode":"custom","allow":["pypi.org"],"enforced":false},"model":null,"cwd":"/work","argv":["/bin/claude","-p"],"env":[]}"#
        );
    }

    #[test]
    fn prompt_and_text_fields_in_order() {
        assert_eq!(
            at(Body::Prompt(Prompt {
                text: "hi".to_string()
            }))
            .to_json(),
            r#"{"schema":1,"type":"prompt","time":"2026-10-04T13:00:00.000Z","text":"hi"}"#
        );
        assert_eq!(
            at(Body::Text(Text {
                parent: Some("t1".to_string()),
                text: "done".to_string()
            }))
            .to_json(),
            r#"{"schema":1,"type":"text","time":"2026-10-04T13:00:00.000Z","parent":"t1","text":"done"}"#
        );
    }

    #[test]
    fn tool_fields_in_order() {
        let event = at(Body::Tool(Tool {
            id: "t1".to_string(),
            parent: None,
            name: "Bash".to_string(),
            summary: "Bash: ls".to_string(),
            denied: true,
        }));
        assert_eq!(
            event.to_json(),
            r#"{"schema":1,"type":"tool","time":"2026-10-04T13:00:00.000Z","id":"t1","parent":null,"name":"Bash","summary":"Bash: ls","denied":true}"#
        );
    }

    #[test]
    fn usage_fields_in_order() {
        let counts = TokenCounts {
            input_tokens: Some(1),
            output_tokens: Some(2),
            cache_read_tokens: Some(3),
            cache_write_tokens: None,
        };
        let event = at(Body::Usage(UsageReport {
            parent: None,
            model: Some("m".to_string()),
            counts,
            context_tokens: counts.context_tokens(),
        }));
        assert_eq!(
            event.to_json(),
            r#"{"schema":1,"type":"usage","time":"2026-10-04T13:00:00.000Z","parent":null,"model":"m","input_tokens":1,"output_tokens":2,"cache_read_tokens":3,"cache_write_tokens":null,"context_tokens":null}"#
        );
    }

    #[test]
    fn subagent_fields_in_order() {
        let start = at(Body::SubagentStart(SubagentStart {
            id: "t1".to_string(),
            parent: None,
            number: 1,
            kind: "Explore".to_string(),
            model: Some("haiku".to_string()),
            description: "look".to_string(),
        }));
        assert_eq!(
            start.to_json(),
            r#"{"schema":1,"type":"subagent_start","time":"2026-10-04T13:00:00.000Z","id":"t1","parent":null,"number":1,"kind":"Explore","model":"haiku","description":"look"}"#
        );
        let end = at(Body::SubagentEnd(SubagentEnd {
            id: "t1".to_string(),
            status: SubagentStatus::Failed,
            duration_ms: 5,
            usage: Usage::default(),
        }));
        assert_eq!(
            end.to_json(),
            r#"{"schema":1,"type":"subagent_end","time":"2026-10-04T13:00:00.000Z","id":"t1","status":"failed","duration_ms":5,"usage":{"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"by_model":{}}}"#
        );
    }

    #[test]
    fn network_fields_in_order() {
        let event = at(Body::Network(Network {
            host: "example.com".to_string(),
            port: 443,
            allowed: false,
            reason: Some(NetworkReason::NotAllowed),
        }));
        assert_eq!(
            event.to_json(),
            r#"{"schema":1,"type":"network","time":"2026-10-04T13:00:00.000Z","host":"example.com","port":443,"allowed":false,"reason":"not_allowed"}"#
        );
    }

    #[test]
    fn end_fields_in_order() {
        let event = at(Body::End(End {
            status: EndStatus::Rejected,
            exit_code: None,
            detail: "bad".to_string(),
            duration_ms: 3,
            usage: Usage::default(),
            result: None,
        }));
        assert_eq!(
            event.to_json(),
            r#"{"schema":1,"type":"end","time":"2026-10-04T13:00:00.000Z","status":"rejected","exit_code":null,"detail":"bad","duration_ms":3,"usage":{"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"by_model":{}},"result":null}"#
        );
    }
}
