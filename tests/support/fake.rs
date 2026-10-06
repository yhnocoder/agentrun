use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agentrun::cli::{RunArgs, Runtime};
use agentrun::network::proxy_environment;
use agentrun::output::{Record, SandboxKind, SubagentStatus, TokenCounts, Usage};
use agentrun::runtime::{Adapter, Failure, Invocation, Launch, PrivateDir};
use agentrun::sandbox;
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

    fn check_args(&self, _args: &RunArgs) -> Result<(), String> {
        Ok(())
    }

    fn launch(&mut self, executable: &Path, invocation: &Invocation) -> Result<Launch, String> {
        let mut argv = vec![executable.to_string_lossy().into_owned()];
        argv.extend(invocation.args.runtime_args.iter().cloned());
        argv.push(invocation.prompt.clone());
        let wrapped = sandbox::wrap(
            &invocation.sandbox,
            &invocation.cwd,
            &invocation.tempdir,
            None,
            invocation.proxy.as_ref(),
            invocation.args.dry_run,
        )?;
        argv = wrapped.argv(&argv);
        let private_dirs = if invocation.runtime == Runtime::Codex && !invocation.args.dry_run {
            let mut path = invocation.tempdir.clone().into_os_string();
            path.push("-codex");
            let path = PathBuf::from(path);
            std::fs::create_dir(&path).map_err(|error| error.to_string())?;
            vec![PrivateDir {
                label: "codex home",
                path,
            }]
        } else {
            Vec::new()
        };
        let port = invocation.proxy.as_ref().map(|proxy| proxy.port_text());
        Ok(Launch {
            argv,
            stdin: format!("{}\n", invocation.prompt).into_bytes(),
            env: port.as_deref().map(proxy_environment).unwrap_or_default(),
            signal_wrapped_child: wrapped.kind == SandboxKind::Bubblewrap,
            service_hosts: Vec::new(),
            wrapped: wrapped.kind,
            private_dirs,
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

    fn failure(&self, exit_code: Option<i32>) -> Option<Failure> {
        match &self.failure {
            Some(detail) => Some(Failure::from_detail(detail.clone())),
            None => (exit_code != Some(0)).then_some(Failure::Unexplained),
        }
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
