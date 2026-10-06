use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::{Adapter, Invocation, Launch};
use crate::cli::{NetworkMode, Runtime};
use crate::json::{first_line, joined_text, optional_string, string};
use crate::network::{HostRule, PORT_PLACEHOLDER, ProxyEndpoint};
use crate::output::{Record, SubagentStatus, TokenCounts, Usage};

const TOOLS: [&str; 7] = ["Read", "Edit", "Write", "Glob", "Grep", "Bash", "Task"];
const ALLOWED_WITHOUT_SANDBOX: [&str; 6] = ["Read", "Edit", "Write", "Glob", "Grep", "Task"];
const ALLOWED_WITH_SANDBOX: [&str; 3] = ["Read", "Glob", "Grep"];
const PATH_TOOLS: [&str; 2] = ["Edit", "Write"];
const WEB_FETCH_TOOL: &str = "WebFetch";
const WEB_SEARCH_TOOL: &str = "WebSearch";
const SOCAT_LISTEN_VARIABLE: &str = "SOCAT_DEFAULT_LISTEN_IP";
const SOCAT_LISTEN_IPV4: &str = "4";
const SUBAGENT_TOOL: &str = "Task";
const SUBAGENT_TOOL_NAMES: [&str; 2] = ["Agent", "Task"];
const HANDBACK_TOOL: &str = "SubagentHandback";
const SYNTHETIC_MODEL: &str = "<synthetic>";
const DEFAULT_SUBAGENT_KIND: &str = "general-purpose";
const NO_RESULT_DETAIL: &str = "runtime exited with code 0 without a result event";
const RANDOM_SOURCE: &str = "/dev/urandom";

#[derive(Default)]
pub struct ClaudeCode {
    cwd: PathBuf,
    no_subagents: bool,
    prompt_echoed: bool,
    subagents: HashSet<String>,
    denied: HashSet<String>,
    pending_usages: Vec<PendingUsage>,
    last_result: Option<Value>,
}

struct PendingUsage {
    parent: Option<String>,
    message_id: String,
    model: Option<String>,
    counts: TokenCounts,
}

#[derive(Serialize)]
struct PromptLine<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    message: PromptMessage<'a>,
}

#[derive(Serialize)]
struct PromptMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Serialize)]
struct Settings<'a> {
    sandbox: SandboxSettings<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SandboxSettings<'a> {
    enabled: bool,
    fail_if_unavailable: bool,
    auto_allow_bash_if_sandboxed: bool,
    allow_unsandboxed_commands: bool,
    filesystem: FilesystemSettings<'a>,
    network: NetworkSettings,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FilesystemSettings<'a> {
    allow_write: [&'a str; 2],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NetworkSettings {
    allowed_domains: [&'static str; 0],
    #[serde(skip_serializing_if = "Option::is_none")]
    http_proxy_port: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    socks_proxy_port: Option<Value>,
}

impl ClaudeCode {
    pub fn new() -> ClaudeCode {
        ClaudeCode::default()
    }

    fn translate_assistant(&mut self, line: &Value) -> Vec<Record> {
        let message = &line["message"];
        let parent = optional_string(&line["parent_tool_use_id"]);
        let synthetic = message["model"].as_str() == Some(SYNTHETIC_MODEL);
        let mut records = Vec::new();
        if !synthetic {
            records.extend(self.note_usage(parent.clone(), message));
        }
        for block in message["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") => records.push(Record::Text {
                    parent: parent.clone(),
                    text: string(&block["text"]),
                }),
                Some("tool_use") if !synthetic => {
                    records.extend(self.tool_use(parent.clone(), block));
                }
                _ => {}
            }
        }
        records
    }

    fn note_usage(&mut self, parent: Option<String>, message: &Value) -> Vec<Record> {
        let message_id = string(&message["id"]);
        let model = optional_string(&message["model"]);
        let counts = counts(&message["usage"]);
        let mut records = Vec::new();
        match self
            .pending_usages
            .iter_mut()
            .find(|pending| pending.parent == parent)
        {
            Some(pending) if pending.message_id == message_id => {
                pending.model = model;
                pending.counts = counts;
            }
            Some(pending) => {
                records.push(usage_record(pending));
                pending.message_id = message_id;
                pending.model = model;
                pending.counts = counts;
            }
            None => self.pending_usages.push(PendingUsage {
                parent,
                message_id,
                model,
                counts,
            }),
        }
        records
    }

    fn flush_usages(&mut self) -> Vec<Record> {
        self.pending_usages
            .drain(..)
            .map(|pending| usage_record(&pending))
            .collect()
    }

    fn tool_use(&mut self, parent: Option<String>, block: &Value) -> Option<Record> {
        let id = string(&block["id"]);
        let name = string(&block["name"]);
        let input = &block["input"];
        if SUBAGENT_TOOL_NAMES.contains(&name.as_str()) {
            self.subagents.insert(id.clone());
            return Some(Record::SubagentStart {
                id,
                parent,
                kind: input["subagent_type"]
                    .as_str()
                    .unwrap_or(DEFAULT_SUBAGENT_KIND)
                    .to_string(),
                model: optional_string(&input["model"]),
                description: first_line(input["description"].as_str().unwrap_or_default()),
            });
        }
        if name == HANDBACK_TOOL {
            return None;
        }
        let summary = tool_summary(&name, input, &self.cwd);
        Some(Record::ToolStart {
            id,
            parent,
            name,
            summary,
        })
    }

    fn translate_user(&mut self, line: &Value) -> Vec<Record> {
        if line["isReplay"] == Value::Bool(true) {
            if self.prompt_echoed {
                return Vec::new();
            }
            self.prompt_echoed = true;
            return vec![Record::PromptEcho {
                text: joined_text(&line["message"]["content"]),
            }];
        }
        let mut records = Vec::new();
        for block in line["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "tool_result")
        {
            let id = string(&block["tool_use_id"]);
            if self.subagents.contains(&id) {
                if line["tool_use_result"]["status"] == "async_launched" {
                    continue;
                }
                let status = if block["is_error"] == Value::Bool(true) {
                    SubagentStatus::Failed
                } else {
                    SubagentStatus::Finished
                };
                records.extend(self.flush_usages());
                records.push(Record::SubagentEnd { id, status });
            } else {
                let denied = self.denied.contains(&id);
                records.push(Record::ToolEnd { id, denied });
            }
        }
        records
    }

    fn translate_system(&mut self, line: &Value) -> Vec<Record> {
        match line["subtype"].as_str() {
            Some("init") => {
                let has_subagent_tool = line["tools"]
                    .as_array()
                    .is_some_and(|tools| tools.iter().any(|tool| tool == SUBAGENT_TOOL));
                if self.no_subagents && has_subagent_tool {
                    vec![Record::Debug(format!(
                        "subagents: {SUBAGENT_TOOL} is in the tool list despite --no-subagents"
                    ))]
                } else {
                    Vec::new()
                }
            }
            Some("permission_denied") => {
                self.denied.insert(string(&line["tool_use_id"]));
                Vec::new()
            }
            Some("task_notification") => {
                let id = string(&line["tool_use_id"]);
                if !self.subagents.contains(&id) {
                    return Vec::new();
                }
                let status = if line["status"] == "completed" {
                    SubagentStatus::Finished
                } else {
                    SubagentStatus::Failed
                };
                let mut records = self.flush_usages();
                records.push(Record::SubagentEnd { id, status });
                records
            }
            _ => Vec::new(),
        }
    }
}

impl Adapter for ClaudeCode {
    fn runtime(&self) -> Runtime {
        Runtime::ClaudeCode
    }

    fn launch(&mut self, executable: &Path, invocation: &Invocation) -> Result<Launch, String> {
        self.cwd = invocation.cwd.clone();
        self.no_subagents = invocation.args.no_subagents;
        let session_id = session_id()?;
        let sandboxed = invocation.sandbox.runs();
        let cwd = invocation.cwd.to_string_lossy();
        let tempdir = invocation.tempdir.to_string_lossy();
        let mut tools: Vec<&str> = TOOLS.to_vec();
        let mut allowed: Vec<String> = if sandboxed {
            let mut rules: Vec<String> = ALLOWED_WITH_SANDBOX
                .iter()
                .map(|tool| tool.to_string())
                .collect();
            for dir in [&cwd, &tempdir] {
                for tool in PATH_TOOLS {
                    rules.push(path_rule(tool, dir));
                }
            }
            rules.push(SUBAGENT_TOOL.to_string());
            rules
        } else {
            ALLOWED_WITHOUT_SANDBOX
                .iter()
                .map(|tool| tool.to_string())
                .collect()
        };
        if invocation.args.no_subagents {
            tools.retain(|tool| *tool != SUBAGENT_TOOL);
            allowed.retain(|tool| tool != SUBAGENT_TOOL);
        }
        match invocation.args.network {
            NetworkMode::None => {}
            NetworkMode::Full => {
                tools.extend([WEB_FETCH_TOOL, WEB_SEARCH_TOOL]);
                allowed.extend([WEB_FETCH_TOOL.to_string(), WEB_SEARCH_TOOL.to_string()]);
            }
            NetworkMode::Custom => {
                tools.push(WEB_FETCH_TOOL);
                allowed.extend(invocation.allow_hosts.iter().map(domain_rule));
            }
        }
        let mut argv = vec![
            executable.to_string_lossy().into_owned(),
            "-p".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--replay-user-messages".to_string(),
            "--permission-mode".to_string(),
            if sandboxed { "dontAsk" } else { "auto" }.to_string(),
            "--setting-sources".to_string(),
            String::new(),
            "--strict-mcp-config".to_string(),
            "--no-session-persistence".to_string(),
            "--session-id".to_string(),
            session_id,
            "--tools".to_string(),
            tools.join(","),
            "--allowedTools".to_string(),
            allowed.join(","),
        ];
        if sandboxed {
            argv.push("--settings".to_string());
            argv.push(sandbox_settings(
                &cwd,
                &tempdir,
                invocation.proxy.as_ref().map(proxy_port),
            ));
        }
        if let Some(model) = &invocation.args.model {
            argv.push("--model".to_string());
            argv.push(model.clone());
        }
        if let Some(effort) = &invocation.args.effort {
            argv.push("--effort".to_string());
            argv.push(effort.clone());
        }
        if let Some(max_turns) = invocation.args.max_turns {
            argv.push("--max-turns".to_string());
            argv.push(max_turns.to_string());
        }
        argv.extend(invocation.args.runtime_args.iter().cloned());
        let env = if sandboxed {
            vec![(
                OsString::from(SOCAT_LISTEN_VARIABLE),
                OsString::from(SOCAT_LISTEN_IPV4),
            )]
        } else {
            Vec::new()
        };
        Ok(Launch {
            argv,
            stdin: prompt_line(&invocation.prompt),
            env,
            signal_wrapped_child: false,
            service_hosts: Vec::new(),
        })
    }

    fn echoes_prompt(&self) -> bool {
        true
    }

    fn translate(&mut self, line: &Value) -> Vec<Record> {
        match line["type"].as_str() {
            Some("assistant") => self.translate_assistant(line),
            Some("user") => self.translate_user(line),
            Some("system") => self.translate_system(line),
            Some("result") => {
                self.last_result = Some(line.clone());
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn after_exit(&mut self) -> Vec<Record> {
        let mut records = self.flush_usages();
        if let Some(result) = &self.last_result {
            if let Some(text) = result["result"].as_str() {
                records.push(Record::Result {
                    text: text.to_string(),
                });
            }
            if let Some(models) = result["modelUsage"].as_object() {
                let by_model: Vec<(String, TokenCounts)> = models
                    .iter()
                    .map(|(model, usage)| (model.clone(), model_usage_counts(usage)))
                    .collect();
                records.push(Record::RunUsage(Usage::sum(
                    by_model
                        .iter()
                        .map(|(model, counts)| (Some(model.as_str()), counts)),
                )));
            }
        }
        records
    }

    fn failure(&self, exit_code: Option<i32>, _stderr_tail: &str) -> Option<String> {
        match &self.last_result {
            Some(result) if result["is_error"] == Value::Bool(true) => {
                Some(result["result"].as_str().unwrap_or_default().to_string())
            }
            _ if exit_code != Some(0) => Some(String::new()),
            None => Some(NO_RESULT_DETAIL.to_string()),
            Some(_) => None,
        }
    }
}

fn path_rule(tool: &str, dir: &str) -> String {
    format!("{tool}(//{}/**)", dir.strip_prefix('/').unwrap_or(dir))
}

fn domain_rule(rule: &HostRule) -> String {
    format!("{WEB_FETCH_TOOL}(domain:{})", rule.pattern())
}

fn proxy_port(proxy: &ProxyEndpoint) -> Value {
    match proxy.port {
        Some(port) => Value::from(port),
        None => Value::from(PORT_PLACEHOLDER),
    }
}

fn sandbox_settings(cwd: &str, tempdir: &str, proxy_port: Option<Value>) -> String {
    serde_json::to_string(&Settings {
        sandbox: SandboxSettings {
            enabled: true,
            fail_if_unavailable: true,
            auto_allow_bash_if_sandboxed: true,
            allow_unsandboxed_commands: false,
            filesystem: FilesystemSettings {
                allow_write: [cwd, tempdir],
            },
            network: NetworkSettings {
                allowed_domains: [],
                http_proxy_port: proxy_port.clone(),
                socks_proxy_port: proxy_port,
            },
        },
    })
    .expect("settings serialize")
}

fn prompt_line(prompt: &str) -> Vec<u8> {
    let mut line = serde_json::to_vec(&PromptLine {
        kind: "user",
        message: PromptMessage {
            role: "user",
            content: prompt,
        },
    })
    .expect("prompt serializes");
    line.push(b'\n');
    line
}

fn session_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open(RANDOM_SOURCE)
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("cannot read {RANDOM_SOURCE} for the session id: {error}"))?;
    Ok(format_uuid_v4(bytes))
}

fn format_uuid_v4(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn tool_summary(name: &str, input: &Value, cwd: &Path) -> String {
    let value = match name {
        "Bash" => input["command"].as_str().map(first_line),
        "Read" | "Edit" | "Write" => input["file_path"]
            .as_str()
            .map(|path| display_path(Path::new(path), cwd)),
        "Glob" | "Grep" => input["pattern"].as_str().map(str::to_string),
        "WebFetch" => input["url"].as_str().map(str::to_string),
        _ => None,
    };
    match value {
        Some(value) => format!("{name}: {value}"),
        None => name.to_string(),
    }
}

fn display_path(path: &Path, cwd: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
        Ok(relative) => relative.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

fn usage_record(pending: &PendingUsage) -> Record {
    Record::Usage {
        parent: pending.parent.clone(),
        model: pending.model.clone(),
        counts: pending.counts,
    }
}

fn counts(usage: &Value) -> TokenCounts {
    TokenCounts {
        input_tokens: usage["input_tokens"].as_u64(),
        output_tokens: usage["output_tokens"].as_u64(),
        cache_read_tokens: usage["cache_read_input_tokens"].as_u64(),
        cache_write_tokens: usage["cache_creation_input_tokens"].as_u64(),
    }
}

fn model_usage_counts(usage: &Value) -> TokenCounts {
    TokenCounts {
        input_tokens: usage["inputTokens"].as_u64(),
        output_tokens: usage["outputTokens"].as_u64(),
        cache_read_tokens: usage["cacheReadInputTokens"].as_u64(),
        cache_write_tokens: usage["cacheCreationInputTokens"].as_u64(),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use serde_json::json;

    use super::*;
    use crate::cli::{Cli, Format, Parsed, SandboxMode};
    use crate::network;
    use crate::sandbox::{Sandbox, Wrapper};
    use crate::session::Session;

    fn invocation(sandboxed: bool, extra: &[&str]) -> Invocation {
        let mut args = vec!["agentrun", "claude-code", "--prompt", "hi"];
        args.extend(extra);
        let (runtime, args) = match Cli::try_parse_from(args).unwrap().command.into_parsed() {
            Parsed::Run(runtime, args) => (runtime, args),
            _ => unreachable!("the tests parse runtime subcommands"),
        };
        Invocation {
            runtime,
            args,
            cwd: PathBuf::from("/work/repo"),
            prompt: "hi".to_string(),
            format: Format::Jsonl,
            sandbox: Sandbox {
                mode: SandboxMode::On,
                wrapper: if sandboxed {
                    Wrapper::Bubblewrap {
                        bwrap: PathBuf::from("/usr/bin/bwrap"),
                        socat: PathBuf::from("/usr/bin/socat"),
                    }
                } else {
                    Wrapper::None
                },
                reason: String::new(),
                description: String::new(),
            },
            tempdir: PathBuf::from("/tmp/agentrun-abc"),
            session: Session::assemble(runtime, &[], &[], &[], &[]),
            allow_hosts: Vec::new(),
            proxy: None,
            codex_home: None,
        }
    }

    fn launch(sandboxed: bool, extra: &[&str]) -> Launch {
        ClaudeCode::new()
            .launch(Path::new("/usr/bin/claude"), &invocation(sandboxed, extra))
            .unwrap()
    }

    fn argv_without_session_id(launch: &Launch) -> Vec<String> {
        let mut argv = launch.argv.clone();
        let index = argv.iter().position(|arg| arg == "--session-id").unwrap();
        argv.remove(index + 1);
        argv
    }

    const FIXED_ARGS: [&str; 15] = [
        "/usr/bin/claude",
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
        "--replay-user-messages",
        "--permission-mode",
        "dontAsk",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--session-id",
    ];

    #[test]
    fn sandboxed_arguments_use_dont_ask_rules_and_settings() {
        let launch = launch(true, &[]);
        let mut expected: Vec<String> = FIXED_ARGS.iter().map(|arg| arg.to_string()).collect();
        expected.extend([
            "--tools".to_string(),
            "Read,Edit,Write,Glob,Grep,Bash,Task".to_string(),
            "--allowedTools".to_string(),
            "Read,Glob,Grep,Edit(//work/repo/**),Write(//work/repo/**),Edit(//tmp/agentrun-abc/**),Write(//tmp/agentrun-abc/**),Task".to_string(),
            "--settings".to_string(),
            r#"{"sandbox":{"enabled":true,"failIfUnavailable":true,"autoAllowBashIfSandboxed":true,"allowUnsandboxedCommands":false,"filesystem":{"allowWrite":["/work/repo","/tmp/agentrun-abc"]},"network":{"allowedDomains":[]}}}"#.to_string(),
        ]);
        assert_eq!(argv_without_session_id(&launch), expected);
    }

    #[test]
    fn unsandboxed_arguments_use_auto_without_settings() {
        let launch = launch(false, &[]);
        let argv = argv_without_session_id(&launch);
        assert_eq!(argv[9], "auto");
        assert_eq!(
            argv[15..],
            [
                "--tools",
                "Read,Edit,Write,Glob,Grep,Bash,Task",
                "--allowedTools",
                "Read,Edit,Write,Glob,Grep,Task",
            ]
        );
        assert!(!argv.contains(&"--settings".to_string()));
    }

    fn proxied(sandboxed: bool, port: Option<u16>, extra: &[&str]) -> Launch {
        let mut invocation = invocation(sandboxed, extra);
        invocation.allow_hosts =
            network::check_usage(invocation.args.network, &invocation.args.allow_host).unwrap();
        invocation.proxy = port.map(|port| ProxyEndpoint {
            port: Some(port),
            socket: PathBuf::from("/tmp/agentrun-abc/proxy.sock"),
        });
        if extra.contains(&"--dry-run") && sandboxed && invocation.args.network != NetworkMode::None
        {
            invocation.proxy = Some(ProxyEndpoint {
                port: None,
                socket: PathBuf::from("/tmp/agentrun-abc/proxy.sock"),
            });
        }
        ClaudeCode::new()
            .launch(Path::new("/usr/bin/claude"), &invocation)
            .unwrap()
    }

    const SOCAT_IPV4: (&str, &str) = ("SOCAT_DEFAULT_LISTEN_IP", "4");

    fn has_env(launch: &Launch, pair: (&str, &str)) -> bool {
        launch
            .env
            .contains(&(OsString::from(pair.0), OsString::from(pair.1)))
    }

    #[test]
    fn full_network_adds_web_tools_and_points_the_sandbox_at_the_proxy() {
        let launch = proxied(true, Some(41234), &["--network", "full"]);
        let argv = argv_without_session_id(&launch);
        assert_eq!(
            argv[16],
            "Read,Edit,Write,Glob,Grep,Bash,Task,WebFetch,WebSearch"
        );
        assert_eq!(
            argv[18],
            "Read,Glob,Grep,Edit(//work/repo/**),Write(//work/repo/**),Edit(//tmp/agentrun-abc/**),Write(//tmp/agentrun-abc/**),Task,WebFetch,WebSearch"
        );
        assert!(
            argv[20].ends_with(
                r#""network":{"allowedDomains":[],"httpProxyPort":41234,"socksProxyPort":41234}}}"#
            ),
            "{}",
            argv[20]
        );
        assert!(has_env(&launch, SOCAT_IPV4));
        assert!(launch.service_hosts.is_empty());
        let open = proxied(false, None, &["--network", "full"]);
        let argv = argv_without_session_id(&open);
        assert_eq!(
            argv[16],
            "Read,Edit,Write,Glob,Grep,Bash,Task,WebFetch,WebSearch"
        );
        assert_eq!(
            argv[18],
            "Read,Edit,Write,Glob,Grep,Task,WebFetch,WebSearch"
        );
        assert!(!has_env(&open, SOCAT_IPV4));
        assert!(open.env.is_empty());
    }

    #[test]
    fn custom_network_allows_web_fetch_per_host_without_ports() {
        let launch = proxied(
            true,
            Some(5000),
            &[
                "--network",
                "custom",
                "--allow-host",
                "Example.com:8443",
                "--allow-host",
                "*.github.com",
            ],
        );
        let argv = argv_without_session_id(&launch);
        assert_eq!(argv[16], "Read,Edit,Write,Glob,Grep,Bash,Task,WebFetch");
        assert!(
            argv[18].ends_with(",Task,WebFetch(domain:example.com),WebFetch(domain:*.github.com)"),
            "{}",
            argv[18]
        );
        assert!(
            argv[20].contains(r#""httpProxyPort":5000,"socksProxyPort":5000"#),
            "{}",
            argv[20]
        );
        assert!(has_env(&launch, SOCAT_IPV4));
    }

    #[test]
    fn no_network_keeps_the_sandbox_settings_and_still_sets_the_socat_family() {
        let launch = proxied(true, None, &[]);
        let argv = argv_without_session_id(&launch);
        assert!(
            argv[20].ends_with(r#""network":{"allowedDomains":[]}}}"#),
            "{}",
            argv[20]
        );
        assert_eq!(
            launch.env,
            vec![(
                OsString::from("SOCAT_DEFAULT_LISTEN_IP"),
                OsString::from("4")
            )]
        );
    }

    #[test]
    fn dry_run_with_network_writes_the_port_placeholder() {
        let launch = proxied(true, None, &["--network", "full", "--dry-run"]);
        let argv = argv_without_session_id(&launch);
        assert!(
            argv[20].ends_with(
                r#""network":{"allowedDomains":[],"httpProxyPort":"<proxy port>","socksProxyPort":"<proxy port>"}}}"#
            ),
            "{}",
            argv[20]
        );
    }

    #[test]
    fn no_subagents_removes_task_from_both_lists() {
        let sandboxed = argv_without_session_id(&launch(true, &["--no-subagents"]));
        assert_eq!(sandboxed[16], "Read,Edit,Write,Glob,Grep,Bash");
        assert_eq!(
            sandboxed[18],
            "Read,Glob,Grep,Edit(//work/repo/**),Write(//work/repo/**),Edit(//tmp/agentrun-abc/**),Write(//tmp/agentrun-abc/**)"
        );
        let open = argv_without_session_id(&launch(false, &["--no-subagents"]));
        assert_eq!(open[16], "Read,Edit,Write,Glob,Grep,Bash");
        assert_eq!(open[18], "Read,Edit,Write,Glob,Grep");
    }

    #[test]
    fn model_effort_max_turns_and_caller_arguments_come_last() {
        let launch = launch(
            false,
            &[
                "--model",
                "sonnet",
                "--effort",
                "high",
                "--max-turns",
                "7",
                "--",
                "--include-partial-messages",
                "--x",
            ],
        );
        let argv = argv_without_session_id(&launch);
        assert_eq!(
            argv[19..],
            [
                "--model",
                "sonnet",
                "--effort",
                "high",
                "--max-turns",
                "7",
                "--include-partial-messages",
                "--x",
            ]
        );
    }

    #[test]
    fn session_id_is_a_lowercase_uuid_v4() {
        let id = session_id().unwrap();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
        assert!(id.chars().all(|c| !c.is_ascii_uppercase()));
        assert_eq!(&id[14..15], "4");
        assert!("89ab".contains(&id[19..20]));
        assert_eq!(
            format_uuid_v4([0xff; 16]),
            "ffffffff-ffff-4fff-bfff-ffffffffffff"
        );
        assert_ne!(session_id().unwrap(), id);
    }

    #[test]
    fn stdin_is_one_json_line_with_the_prompt_as_is() {
        let prompt = "line \"one\"\n第二行 \\ 'q'\t\n";
        let mut invocation = invocation(false, &[]);
        invocation.prompt = prompt.to_string();
        let launch = ClaudeCode::new()
            .launch(Path::new("/usr/bin/claude"), &invocation)
            .unwrap();
        let text = String::from_utf8(launch.stdin).unwrap();
        assert_eq!(
            text,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"line \\\"one\\\"\\n第二行 \\\\ 'q'\\t\\n\"}}\n"
        );
        let parsed: Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(parsed["message"]["content"], prompt);
    }

    #[test]
    fn summary_per_tool_and_path_rules() {
        let cwd = Path::new("/a/b");
        let cases = [
            (
                "Bash",
                json!({"command": "cargo test\necho done"}),
                "Bash: cargo test",
            ),
            (
                "Read",
                json!({"file_path": "/a/b/src/main.rs"}),
                "Read: src/main.rs",
            ),
            (
                "Edit",
                json!({"file_path": "/a/bc/x.rs"}),
                "Edit: /a/bc/x.rs",
            ),
            (
                "Write",
                json!({"file_path": "/etc/hosts"}),
                "Write: /etc/hosts",
            ),
            ("Write", json!({"file_path": "/a/b"}), "Write: ."),
            ("Glob", json!({"pattern": "**/*.rs"}), "Glob: **/*.rs"),
            ("Grep", json!({"pattern": "fn main"}), "Grep: fn main"),
            (
                "WebFetch",
                json!({"url": "https://example.com/"}),
                "WebFetch: https://example.com/",
            ),
            ("Skill", json!({"skill": "x"}), "Skill"),
            ("Bash", json!({}), "Bash"),
            ("Read", json!({"file_path": 3}), "Read"),
        ];
        for (name, input, expected) in cases {
            assert_eq!(tool_summary(name, &input, cwd), expected, "{name} {input}");
        }
    }

    fn assistant(parent: Option<&str>, id: &str, model: &str, block: Value, output: u64) -> Value {
        json!({
            "type": "assistant",
            "parent_tool_use_id": parent,
            "message": {
                "id": id,
                "model": model,
                "content": [block],
                "usage": {"input_tokens": 1, "output_tokens": output, "cache_read_input_tokens": 2, "cache_creation_input_tokens": 3}
            }
        })
    }

    fn usage(parent: Option<&str>, output: u64) -> Record {
        Record::Usage {
            parent: parent.map(str::to_string),
            model: Some("claude-sonnet-5-5".to_string()),
            counts: TokenCounts {
                input_tokens: Some(1),
                output_tokens: Some(output),
                cache_read_tokens: Some(2),
                cache_write_tokens: Some(3),
            },
        }
    }

    #[test]
    fn same_message_id_outputs_one_usage_with_the_last_values() {
        let mut adapter = ClaudeCode::new();
        let thinking = json!({"type": "thinking", "thinking": ""});
        let text = json!({"type": "text", "text": "hi"});
        assert_eq!(
            adapter.translate(&assistant(None, "m1", "claude-sonnet-5-5", thinking, 5)),
            vec![]
        );
        assert_eq!(
            adapter.translate(&assistant(None, "m1", "claude-sonnet-5-5", text.clone(), 9)),
            vec![Record::Text {
                parent: None,
                text: "hi".to_string()
            }]
        );
        assert_eq!(
            adapter.translate(&assistant(None, "m2", "claude-sonnet-5-5", text.clone(), 1)),
            vec![
                usage(None, 9),
                Record::Text {
                    parent: None,
                    text: "hi".to_string()
                }
            ]
        );
        assert_eq!(
            adapter.translate(&assistant(None, "s1", SYNTHETIC_MODEL, text, 0)),
            vec![Record::Text {
                parent: None,
                text: "hi".to_string()
            }]
        );
        assert_eq!(adapter.after_exit(), vec![usage(None, 1)]);
        assert_eq!(adapter.after_exit(), vec![]);
    }

    #[test]
    fn usage_is_merged_per_parent_and_flushed_before_subagent_end() {
        let mut adapter = ClaudeCode::new();
        let agent = |id: &str| json!({"type": "tool_use", "id": id, "name": "Agent", "input": {"description": "look\nmore", "subagent_type": "Explore", "model": "haiku"}});
        let text = json!({"type": "text", "text": "t"});
        let records = adapter.translate(&assistant(None, "m1", "claude-sonnet-5-5", agent("A"), 1));
        assert_eq!(
            records,
            vec![Record::SubagentStart {
                id: "A".to_string(),
                parent: None,
                kind: "Explore".to_string(),
                model: Some("haiku".to_string()),
                description: "look".to_string(),
            }]
        );
        let records = adapter.translate(&assistant(None, "m1", "claude-sonnet-5-5", agent("B"), 2));
        assert_eq!(records.len(), 1);
        assert_eq!(
            adapter.translate(&assistant(
                Some("A"),
                "a1",
                "claude-sonnet-5-5",
                text.clone(),
                3
            )),
            vec![Record::Text {
                parent: Some("A".to_string()),
                text: "t".to_string()
            }]
        );
        assert_eq!(
            adapter.translate(&assistant(
                Some("B"),
                "b1",
                "claude-sonnet-5-5",
                text.clone(),
                4
            )),
            vec![Record::Text {
                parent: Some("B".to_string()),
                text: "t".to_string()
            }]
        );
        assert_eq!(
            adapter.translate(&assistant(Some("A"), "a1", "claude-sonnet-5-5", text, 5)),
            vec![Record::Text {
                parent: Some("A".to_string()),
                text: "t".to_string()
            }]
        );
        let notification = json!({"type": "system", "subtype": "task_notification", "tool_use_id": "A", "status": "completed"});
        assert_eq!(
            adapter.translate(&notification),
            vec![
                usage(None, 2),
                usage(Some("A"), 5),
                usage(Some("B"), 4),
                Record::SubagentEnd {
                    id: "A".to_string(),
                    status: SubagentStatus::Finished
                }
            ]
        );
        let bash_notification = json!({"type": "system", "subtype": "task_notification", "tool_use_id": "bash-task", "status": "completed"});
        assert_eq!(adapter.translate(&bash_notification), vec![]);
        let launched = json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "B"}]}, "tool_use_result": {"status": "async_launched"}});
        assert_eq!(adapter.translate(&launched), vec![]);
        let failed = json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "B", "is_error": true}]}});
        assert_eq!(
            adapter.translate(&failed),
            vec![Record::SubagentEnd {
                id: "B".to_string(),
                status: SubagentStatus::Failed
            }]
        );
    }

    #[test]
    fn denied_tools_prompt_echo_and_init_debug_line() {
        let mut adapter = ClaudeCode::new();
        adapter.no_subagents = true;
        let init = json!({"type": "system", "subtype": "init", "tools": ["Task", "Bash"]});
        assert_eq!(
            adapter.translate(&init),
            vec![Record::Debug(
                "subagents: Task is in the tool list despite --no-subagents".to_string()
            )]
        );
        let replay = json!({"type": "user", "isReplay": true, "message": {"role": "user", "content": "do it"}});
        assert_eq!(
            adapter.translate(&replay),
            vec![Record::PromptEcho {
                text: "do it".to_string()
            }]
        );
        assert_eq!(adapter.translate(&replay), vec![]);
        let denied = json!({"type": "system", "subtype": "permission_denied", "tool_use_id": "t1"});
        assert_eq!(adapter.translate(&denied), vec![]);
        let result = |id: &str| json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "is_error": true}]}});
        assert_eq!(
            adapter.translate(&result("t1")),
            vec![Record::ToolEnd {
                id: "t1".to_string(),
                denied: true
            }]
        );
        assert_eq!(
            adapter.translate(&result("t2")),
            vec![Record::ToolEnd {
                id: "t2".to_string(),
                denied: false
            }]
        );
        let handback = assistant(
            Some("A"),
            "m",
            "claude-sonnet-5-5",
            json!({"type": "tool_use", "id": "h", "name": "SubagentHandback", "input": {}}),
            1,
        );
        assert_eq!(adapter.translate(&handback), vec![]);
    }

    #[test]
    fn result_gives_result_text_and_run_usage() {
        let mut adapter = ClaudeCode::new();
        let first =
            json!({"type": "result", "is_error": false, "result": "early", "modelUsage": {}});
        let last = json!({"type": "result", "is_error": false, "result": "final", "modelUsage": {
            "claude-sonnet-5-5": {"inputTokens": 1, "outputTokens": 2, "cacheReadInputTokens": 3, "cacheCreationInputTokens": 4},
            "claude-haiku-4-5": {"inputTokens": 10, "outputTokens": 20}
        }});
        assert_eq!(adapter.translate(&first), vec![]);
        assert_eq!(adapter.translate(&last), vec![]);
        let records = adapter.after_exit();
        assert_eq!(
            records[0],
            Record::Result {
                text: "final".to_string()
            }
        );
        let Record::RunUsage(usage) = &records[1] else {
            panic!("{records:?}");
        };
        assert_eq!(usage.totals.input_tokens, Some(11));
        assert_eq!(usage.totals.output_tokens, Some(22));
        assert_eq!(usage.totals.cache_read_tokens, Some(3));
        assert_eq!(usage.totals.cache_write_tokens, Some(4));
        assert_eq!(usage.by_model["claude-haiku-4-5"].cache_read_tokens, None);
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn failure_cases_in_order() {
        let mut adapter = ClaudeCode::new();
        assert_eq!(
            adapter.failure(Some(0), ""),
            Some(NO_RESULT_DETAIL.to_string())
        );
        assert_eq!(adapter.failure(Some(1), "err"), Some(String::new()));
        assert_eq!(adapter.failure(None, "err"), Some(String::new()));
        adapter.translate(&json!({"type": "result", "is_error": false, "result": "ok"}));
        assert_eq!(adapter.failure(Some(0), ""), None);
        assert_eq!(adapter.failure(Some(2), ""), Some(String::new()));
        adapter.translate(&json!({"type": "result", "is_error": true, "result": "bad model"}));
        assert_eq!(adapter.failure(Some(0), ""), Some("bad model".to_string()));
        assert_eq!(adapter.failure(Some(1), "x"), Some("bad model".to_string()));
        adapter.translate(&json!({"type": "result", "is_error": true}));
        assert_eq!(adapter.failure(Some(0), ""), Some(String::new()));
    }
}
