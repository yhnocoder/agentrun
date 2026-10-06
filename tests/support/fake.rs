use std::collections::BTreeMap;
use std::path::Path;

use agentrun::cli::Runtime;
use agentrun::network::proxy_environment;
use agentrun::output::{SandboxKind, SubagentStatus, TokenCounts, Usage};
use agentrun::runtime::{Adapter, Invocation, Launch, Record};
use agentrun::sandbox::{ProxyForward, wrap_pi, wrap_seatbelt, write_seatbelt_profile};
use serde_json::Value;

pub struct FakeAdapter {
    echoes: bool,
    failure: Option<String>,
}

impl FakeAdapter {
    pub fn new(echoes: bool) -> FakeAdapter {
        FakeAdapter {
            echoes,
            failure: None,
        }
    }
}

impl Adapter for FakeAdapter {
    fn runtime(&self) -> Runtime {
        Runtime::ClaudeCode
    }

    fn launch(&mut self, executable: &Path, invocation: &Invocation) -> Result<Launch, String> {
        let mut argv = vec![executable.to_string_lossy().into_owned()];
        argv.extend(invocation.args.runtime_args.iter().cloned());
        argv.push(invocation.prompt.clone());
        let bwrap = invocation
            .sandbox
            .bwrap
            .as_deref()
            .filter(|_| invocation.sandbox.runs());
        let port = invocation.proxy.as_ref().map(|proxy| proxy.port_text());
        if let Some(bwrap) = bwrap {
            let forward = match (&invocation.proxy, &port, &invocation.sandbox.socat) {
                (Some(proxy), Some(port), Some(socat)) => Some(ProxyForward {
                    socat,
                    port,
                    socket: &proxy.socket,
                }),
                _ => None,
            };
            argv = wrap_pi(
                bwrap,
                &invocation.cwd,
                &invocation.tempdir,
                None,
                forward.as_ref(),
                &argv,
            );
        } else if invocation.sandbox.kind == SandboxKind::Seatbelt {
            if !invocation.args.dry_run {
                write_seatbelt_profile(&invocation.cwd, &invocation.tempdir, None, port.as_deref())
                    .map_err(|error| error.to_string())?;
            }
            argv = wrap_seatbelt(&invocation.tempdir, &argv);
        }
        Ok(Launch {
            argv,
            stdin: format!("{}\n", invocation.prompt).into_bytes(),
            env: port.as_deref().map(proxy_environment).unwrap_or_default(),
            signal_wrapped_child: bwrap.is_some(),
            service_hosts: Vec::new(),
        })
    }

    fn echoes_prompt(&self) -> bool {
        self.echoes
    }

    fn translate(&mut self, line: &Value) -> Vec<Record> {
        let record = match line["record"].as_str() {
            Some("prompt_echo") => Record::PromptEcho {
                text: string(line, "text"),
            },
            Some("tool_start") => Record::ToolStart {
                id: string(line, "id"),
                parent: optional(line, "parent"),
                name: string(line, "name"),
                summary: string(line, "summary"),
            },
            Some("tool_end") => Record::ToolEnd {
                id: string(line, "id"),
                denied: line["denied"].as_bool().unwrap_or(false),
            },
            Some("subagent_start") => Record::SubagentStart {
                id: string(line, "id"),
                parent: optional(line, "parent"),
                kind: string(line, "kind"),
                model: optional(line, "model"),
                description: string(line, "description"),
            },
            Some("subagent_end") => Record::SubagentEnd {
                id: string(line, "id"),
                status: if line["status"] == "finished" {
                    SubagentStatus::Finished
                } else {
                    SubagentStatus::Failed
                },
            },
            Some("text") => Record::Text {
                parent: optional(line, "parent"),
                text: string(line, "text"),
            },
            Some("usage") => Record::Usage {
                parent: optional(line, "parent"),
                model: optional(line, "model"),
                counts: counts(line),
            },
            Some("run_usage") => Record::RunUsage(Usage {
                totals: counts(line),
                by_model: line["by_model"]
                    .as_object()
                    .map(|models| {
                        models
                            .iter()
                            .map(|(model, value)| (model.clone(), counts(value)))
                            .collect::<BTreeMap<_, _>>()
                    })
                    .unwrap_or_default(),
            }),
            Some("result") => Record::Result {
                text: string(line, "text"),
            },
            Some("fail") => {
                self.failure = Some(string(line, "detail"));
                return Vec::new();
            }
            Some("terminate") => {
                self.failure = Some(string(line, "detail"));
                Record::Terminate
            }
            _ => return Vec::new(),
        };
        vec![record]
    }

    fn after_exit(&mut self) -> Vec<Record> {
        Vec::new()
    }

    fn failure(&self, exit_code: Option<i32>, _stderr_tail: &str) -> Option<String> {
        self.failure
            .clone()
            .or_else(|| (exit_code != Some(0)).then(String::new))
    }
}

fn string(line: &Value, key: &str) -> String {
    line[key].as_str().unwrap_or_default().to_string()
}

fn optional(line: &Value, key: &str) -> Option<String> {
    line[key].as_str().map(str::to_string)
}

fn counts(line: &Value) -> TokenCounts {
    TokenCounts {
        input_tokens: line["input_tokens"].as_u64(),
        output_tokens: line["output_tokens"].as_u64(),
        cache_read_tokens: line["cache_read_tokens"].as_u64(),
        cache_write_tokens: line["cache_write_tokens"].as_u64(),
    }
}
