mod support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentrun::adapter::{Adapter, Launch, Record};
use agentrun::cli::Runtime;
use agentrun::event::SubagentStatus;
use agentrun::run::{Caller, Invocation, run};
use agentrun::usage::{TokenCounts, Usage};
use serde_json::{Value, json};
use tempfile::TempDir;

struct FakeAdapter {
    echoes: bool,
    failure: Option<String>,
}

impl FakeAdapter {
    fn new(echoes: bool) -> FakeAdapter {
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

    fn launch(&self, executable: &Path, invocation: &Invocation) -> Launch {
        let mut argv = vec![executable.to_string_lossy().into_owned()];
        argv.extend(invocation.args.runtime_args.iter().cloned());
        argv.push(invocation.prompt.clone());
        Launch {
            argv,
            stdin: format!("{}\n", invocation.prompt).into_bytes(),
        }
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
            _ => return Vec::new(),
        };
        vec![record]
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

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new(script: &str) -> Sandbox {
        let sandbox = Sandbox {
            root: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir(sandbox.bin()).unwrap();
        std::fs::create_dir(sandbox.tmp()).unwrap();
        std::fs::create_dir(sandbox.work()).unwrap();
        sandbox.install("claude", script);
        sandbox
    }

    fn install(&self, name: &str, script: &str) {
        let path = self.bin().join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn with_output(lines: &str) -> Sandbox {
        Sandbox::new(&format!("#!/bin/sh\ncat <<'EOF'\n{lines}\nEOF\n"))
    }

    fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    fn tmp(&self) -> PathBuf {
        self.root.path().join("tmp")
    }

    fn work(&self) -> PathBuf {
        self.root.path().join("work")
    }

    fn path_var(&self) -> String {
        format!("{}:/usr/bin:/bin", self.bin().display())
    }

    fn leftover_tempdirs(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.tmp())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("agentrun-")
            })
            .collect()
    }

    fn run(&self, extra: &[&str], echoes: bool) -> Outcome {
        self.run_as("claude-code", &[], extra, echoes)
    }

    fn run_as(
        &self,
        runtime: &str,
        caller_env: &[(&str, &str)],
        extra: &[&str],
        echoes: bool,
    ) -> Outcome {
        let mut args: Vec<OsString> = vec![
            "agentrun".into(),
            runtime.into(),
            "--cwd".into(),
            self.work().into(),
            "--prompt".into(),
            "hi".into(),
        ];
        args.extend(extra.iter().map(OsString::from));
        if !extra.contains(&"--sandbox") {
            args.splice(2..2, ["--sandbox".into(), "off".into()]);
        }
        let mut env = vec![
            (OsString::from("PATH"), OsString::from(self.path_var())),
            (OsString::from("TMPDIR"), self.tmp().into_os_string()),
        ];
        env.extend(
            caller_env
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        );
        let stdout = Shared::default();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let caller = Caller {
            args,
            env,
            stdin: Box::new(std::io::empty()),
            stdin_is_terminal: false,
            stdout: Box::new(stdout.clone()),
            stdout_is_terminal: false,
            stderr: stderr.clone(),
        };
        let code = run(caller, &move |_| {
            Some(Box::new(FakeAdapter::new(echoes)) as Box<dyn Adapter>)
        });
        let stderr = String::from_utf8(stderr.lock().unwrap().clone()).unwrap();
        Outcome {
            code,
            stdout: stdout.text(),
            stderr,
        }
    }
}

struct Outcome {
    code: u8,
    stdout: String,
    stderr: String,
}

impl Outcome {
    fn events(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn stripped(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .map(support::without_timing)
            .collect()
    }

    fn end(&self) -> Value {
        self.events().pop().unwrap()
    }
}

fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|event| event["type"].as_str().unwrap().to_string())
        .collect()
}

fn empty_usage() -> Value {
    json!({"input_tokens": null, "output_tokens": null, "cache_read_tokens": null, "cache_write_tokens": null, "by_model": {}})
}

#[test]
fn normal_run_without_echo_outputs_prompt_after_start() {
    let sandbox = Sandbox::new(concat!(
        "#!/bin/sh\n",
        "read -r line\n",
        "printf '{\"record\":\"tool_start\",\"id\":\"t1\",\"parent\":null,\"name\":\"Bash\",\"summary\":\"Bash: ls\"}\\n'\n",
        "printf '{\"record\":\"tool_end\",\"id\":\"t1\",\"denied\":true}\\n'\n",
        "printf '{\"record\":\"text\",\"parent\":null,\"text\":\"stdin=%s\"}\\n' \"$line\"\n",
        "printf '{\"record\":\"usage\",\"parent\":null,\"model\":\"m1\",\"input_tokens\":10,\"output_tokens\":2,\"cache_read_tokens\":5,\"cache_write_tokens\":1}\\n'\n",
        "printf '{\"record\":\"usage\",\"parent\":null,\"model\":null,\"input_tokens\":3,\"output_tokens\":4,\"cache_read_tokens\":null,\"cache_write_tokens\":null}\\n'\n",
        "printf '{\"record\":\"text\",\"parent\":null,\"text\":\"all done\"}\\n'\n",
    ));
    let outcome = sandbox.run(
        &[
            "--model",
            "m1",
            "--network",
            "custom",
            "--allow-host",
            "pypi.org",
            "--",
            "--extra",
        ],
        false,
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let events = outcome.events();
    for event in &events {
        assert_eq!(event["schema"], 1);
        let time = event["time"].as_str().unwrap();
        assert_eq!(time.len(), 24);
        assert!(time.ends_with('Z'));
    }
    let executable = sandbox.bin().join("claude").to_string_lossy().into_owned();
    let work = sandbox.work().to_string_lossy().into_owned();
    assert_eq!(
        outcome.stripped(),
        vec![
            json!({"schema": 1, "type": "start", "runtime": "claude-code", "sandbox": "none",
                "network": {"mode": "custom", "allow": ["pypi.org"], "enforced": false},
                "model": "m1", "cwd": work, "argv": [executable, "--extra"], "env": []}),
            json!({"schema": 1, "type": "prompt", "text": "hi"}),
            json!({"schema": 1, "type": "tool", "id": "t1", "parent": null, "name": "Bash", "summary": "Bash: ls", "denied": true}),
            json!({"schema": 1, "type": "text", "parent": null, "text": "stdin=hi"}),
            json!({"schema": 1, "type": "usage", "parent": null, "model": "m1", "input_tokens": 10, "output_tokens": 2,
                "cache_read_tokens": 5, "cache_write_tokens": 1, "context_tokens": 16}),
            json!({"schema": 1, "type": "usage", "parent": null, "model": null, "input_tokens": 3, "output_tokens": 4,
                "cache_read_tokens": null, "cache_write_tokens": null, "context_tokens": null}),
            json!({"schema": 1, "type": "text", "parent": null, "text": "all done"}),
            json!({"schema": 1, "type": "end", "status": "finished", "exit_code": 0, "detail": "",
                "usage": {"input_tokens": 13, "output_tokens": 6, "cache_read_tokens": 5, "cache_write_tokens": 1,
                    "by_model": {
                        "m1": {"input_tokens": 10, "output_tokens": 2, "cache_read_tokens": 5, "cache_write_tokens": 1},
                        "unknown": {"input_tokens": 3, "output_tokens": 4, "cache_read_tokens": null, "cache_write_tokens": null}
                    }},
                "result": "all done"}),
        ]
    );
    assert!(sandbox.leftover_tempdirs().is_empty());
}

#[test]
fn normal_run_with_echo_uses_echo_result_and_run_usage() {
    let sandbox = Sandbox::new(concat!(
        "#!/bin/sh\n",
        "read -r line\n",
        "printf '{\"record\":\"prompt_echo\",\"text\":\"%s\"}\\n' \"$line\"\n",
        "printf '{\"record\":\"text\",\"parent\":null,\"text\":\"first\"}\\n'\n",
        "printf '{\"record\":\"usage\",\"parent\":null,\"model\":\"m1\",\"input_tokens\":1,\"output_tokens\":1,\"cache_read_tokens\":0,\"cache_write_tokens\":0}\\n'\n",
        "printf '{\"record\":\"result\",\"text\":\"final answer\"}\\n'\n",
        "printf '{\"record\":\"text\",\"parent\":null,\"text\":\"later\"}\\n'\n",
        "printf '{\"record\":\"run_usage\",\"input_tokens\":7,\"output_tokens\":8,\"cache_read_tokens\":null,\"cache_write_tokens\":9,\"by_model\":{\"m1\":{\"input_tokens\":7,\"output_tokens\":8,\"cache_write_tokens\":9}}}\\n'\n",
    ));
    let outcome = sandbox.run(&[], true);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let events = outcome.stripped();
    assert_eq!(
        types(&events),
        vec!["start", "prompt", "text", "usage", "text", "end"]
    );
    assert_eq!(events[1]["text"], "hi");
    assert_eq!(
        events[0]["argv"],
        json!([sandbox.bin().join("claude").to_string_lossy()])
    );
    let end = &events[5];
    assert_eq!(end["result"], "final answer");
    assert_eq!(
        end["usage"],
        json!({"input_tokens": 7, "output_tokens": 8, "cache_read_tokens": null, "cache_write_tokens": 9,
            "by_model": {"m1": {"input_tokens": 7, "output_tokens": 8, "cache_read_tokens": null, "cache_write_tokens": 9}}})
    );
}

#[test]
fn nested_subagents_get_numbers_parents_and_usage() {
    let sandbox = Sandbox::with_output(concat!(
        r#"{"record":"subagent_start","id":"s1","parent":null,"kind":"Explore","model":"haiku","description":"look"}"#,
        "\n",
        r#"{"record":"subagent_start","id":"s2","parent":"s1","kind":"general-purpose","model":null,"description":"dig"}"#,
        "\n",
        r#"{"record":"subagent_start","id":"s3","parent":"nobody","kind":"Plan","model":null,"description":"plan"}"#,
        "\n",
        r#"{"record":"tool_start","id":"t1","parent":"s2","name":"Read","summary":"Read: a.rs"}"#,
        "\n",
        r#"{"record":"tool_end","id":"t1","denied":false}"#,
        "\n",
        r#"{"record":"tool_end","id":"t1","denied":false}"#,
        "\n",
        r#"{"record":"text","parent":"ghost","text":"orphan"}"#,
        "\n",
        r#"{"record":"usage","parent":null,"model":"main","input_tokens":100,"output_tokens":10,"cache_read_tokens":0,"cache_write_tokens":0}"#,
        "\n",
        r#"{"record":"usage","parent":"s1","model":"haiku-4","input_tokens":20,"output_tokens":2,"cache_read_tokens":null,"cache_write_tokens":null}"#,
        "\n",
        r#"{"record":"usage","parent":"s2","model":null,"input_tokens":5,"output_tokens":1,"cache_read_tokens":3,"cache_write_tokens":null}"#,
        "\n",
        r#"{"record":"subagent_end","id":"s2","status":"finished"}"#,
        "\n",
        r#"{"record":"subagent_end","id":"s2","status":"finished"}"#,
        "\n",
        r#"{"record":"subagent_end","id":"s1","status":"failed"}"#,
        "\n",
        r#"{"record":"subagent_end","id":"s3","status":"finished"}"#
    ));
    let outcome = sandbox.run(&[], false);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let events = outcome.stripped();
    assert_eq!(
        types(&events),
        vec![
            "start",
            "prompt",
            "subagent_start",
            "subagent_start",
            "subagent_start",
            "tool",
            "text",
            "usage",
            "usage",
            "usage",
            "subagent_end",
            "subagent_end",
            "subagent_end",
            "end"
        ]
    );
    assert_eq!(
        events[2],
        json!({"schema": 1, "type": "subagent_start", "id": "s1", "parent": null, "number": 1, "kind": "Explore", "model": "haiku", "description": "look"})
    );
    assert_eq!(events[3]["parent"], "s1");
    assert_eq!(events[3]["number"], 2);
    assert_eq!(events[4]["parent"], Value::Null);
    assert_eq!(events[4]["number"], 3);
    assert_eq!(events[5]["parent"], "s2");
    assert_eq!(events[6]["parent"], Value::Null);
    assert_eq!(events[6]["text"], "orphan");
    assert_eq!(
        events[10],
        json!({"schema": 1, "type": "subagent_end", "id": "s2", "status": "finished",
            "usage": {"input_tokens": 5, "output_tokens": 1, "cache_read_tokens": 3, "cache_write_tokens": null,
                "by_model": {"unknown": {"input_tokens": 5, "output_tokens": 1, "cache_read_tokens": 3, "cache_write_tokens": null}}}})
    );
    assert_eq!(
        events[11],
        json!({"schema": 1, "type": "subagent_end", "id": "s1", "status": "failed",
        "usage": {"input_tokens": 25, "output_tokens": 3, "cache_read_tokens": 3, "cache_write_tokens": null,
            "by_model": {
                "haiku-4": {"input_tokens": 20, "output_tokens": 2, "cache_read_tokens": null, "cache_write_tokens": null},
                "unknown": {"input_tokens": 5, "output_tokens": 1, "cache_read_tokens": 3, "cache_write_tokens": null}
            }}})
    );
    assert_eq!(events[12]["id"], "s3");
    assert_eq!(events[12]["usage"], empty_usage());
    assert_eq!(events[13]["usage"]["input_tokens"], 125);
    assert_eq!(events[13]["result"], "orphan");
}

#[test]
fn unfinished_tools_and_subagents_are_closed_before_end() {
    let sandbox = Sandbox::with_output(concat!(
        r#"{"record":"tool_start","id":"a","parent":null,"name":"Bash","summary":"Bash: sleep"}"#,
        "\n",
        r#"{"record":"subagent_start","id":"s1","parent":null,"kind":"Explore","model":null,"description":"x"}"#,
        "\n",
        r#"{"record":"subagent_start","id":"s2","parent":"s1","kind":"Explore","model":null,"description":"y"}"#,
        "\n",
        r#"{"record":"tool_start","id":"b","parent":"s1","name":"Grep","summary":"Grep: foo"}"#,
        "\n",
        r#"{"record":"usage","parent":"s2","model":"m","input_tokens":4,"output_tokens":1,"cache_read_tokens":0,"cache_write_tokens":0}"#
    ));
    let outcome = sandbox.run(&[], false);
    let events = outcome.stripped();
    assert_eq!(
        types(&events),
        vec![
            "start",
            "prompt",
            "subagent_start",
            "subagent_start",
            "usage",
            "tool",
            "tool",
            "subagent_end",
            "subagent_end",
            "end"
        ]
    );
    assert_eq!(
        events[5],
        json!({"schema": 1, "type": "tool", "id": "a", "parent": null, "name": "Bash", "summary": "Bash: sleep", "denied": false})
    );
    assert_eq!(
        events[6],
        json!({"schema": 1, "type": "tool", "id": "b", "parent": "s1", "name": "Grep", "summary": "Grep: foo", "denied": false})
    );
    assert_eq!(events[7]["id"], "s1");
    assert_eq!(events[7]["status"], "failed");
    assert_eq!(events[7]["usage"]["input_tokens"], 4);
    assert!(outcome.events()[7]["duration_ms"].is_u64());
    assert_eq!(events[8]["id"], "s2");
    assert_eq!(events[8]["status"], "failed");
    assert_eq!(events[9]["status"], "finished");
}

#[test]
fn raw_file_matches_runtime_stdout_and_skips_non_json() {
    let output = concat!(
        "not json at all\n",
        r#"{"record":"text","parent":null,"text":"hello"}"#,
        "\n",
        "\n",
        r#"{"record":"unknown"}"#,
        "\n",
        "{broken"
    );
    let sandbox = Sandbox::with_output(output);
    let raw = sandbox.root.path().join("raw.jsonl");
    let outcome = sandbox.run(&["--raw", raw.to_str().unwrap()], false);
    assert_eq!(
        types(&outcome.events()),
        vec!["start", "prompt", "text", "end"]
    );
    assert_eq!(
        std::fs::read_to_string(&raw).unwrap(),
        format!("{output}\n")
    );
}

#[test]
fn nonzero_exit_fails_with_stderr_tail_and_forwards_stderr() {
    let noise = "a".repeat(100) + &"é".repeat(500);
    let sandbox = Sandbox::new(&format!(
        "#!/bin/sh\ncat >&2 <<'EOF'\n{noise}\nEOF\nexit 3\n"
    ));
    let outcome = sandbox.run(&[], false);
    assert_eq!(outcome.code, 1);
    assert_eq!(outcome.stderr, format!("{noise}\n"));
    let end = outcome.end();
    assert_eq!(end["status"], "failed");
    assert_eq!(end["exit_code"], 3);
    assert_eq!(end["detail"], "é".repeat(500));
}

#[test]
fn adapter_failure_detail_comes_first() {
    let sandbox = Sandbox::new(concat!(
        "#!/bin/sh\n",
        "printf '{\"record\":\"fail\",\"detail\":\"adapter says no\"}\\n'\n",
        "echo 'stderr text' >&2\n",
        "exit 1\n",
    ));
    let end = sandbox.run(&[], false).end();
    assert_eq!(end["status"], "failed");
    assert_eq!(end["detail"], "adapter says no");
}

#[test]
fn adapter_failure_on_success_exit_also_fails() {
    let sandbox = Sandbox::with_output(r#"{"record":"fail","detail":"no result"}"#);
    let outcome = sandbox.run(&[], false);
    assert_eq!(outcome.code, 1);
    assert_eq!(outcome.end()["exit_code"], 0);
    assert_eq!(outcome.end()["detail"], "no result");
}

#[test]
fn failure_without_any_message_uses_fixed_sentence() {
    let sandbox = Sandbox::new("#!/bin/sh\nexit 1\n");
    let end = sandbox.run(&[], false).end();
    assert_eq!(end["detail"], "claude-code failed without an error message");
}

#[test]
fn child_sees_session_tempdir_which_is_removed_afterwards() {
    let sandbox = Sandbox::new(concat!(
        "#!/bin/sh\n",
        "mode=$(ls -ld \"$TMPDIR\" | cut -c1-10)\n",
        "printf '{\"record\":\"text\",\"parent\":null,\"text\":\"%s|%s|%s|%s\"}\\n' \"$TMPDIR\" \"$TMP\" \"$TEMP\" \"$mode\"\n",
    ));
    let outcome = sandbox.run(&[], false);
    let text = outcome.events()[2]["text"].as_str().unwrap().to_string();
    let parts: Vec<&str> = text.split('|').collect();
    assert_eq!(parts[0], parts[1]);
    assert_eq!(parts[0], parts[2]);
    assert_eq!(parts[3], "drwx------");
    let dir = Path::new(parts[0]);
    assert_eq!(dir.parent().unwrap(), sandbox.tmp());
    assert!(
        dir.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("agentrun-")
    );
    assert!(!dir.exists());
    assert!(sandbox.leftover_tempdirs().is_empty());
}

#[test]
fn debug_keeps_tempdir_and_writes_raw_jsonl() {
    let output = concat!(
        "plain line\n",
        r#"{"record":"text","parent":null,"text":"hello"}"#
    );
    let sandbox = Sandbox::with_output(output);
    let outcome = sandbox.run(&["--debug"], false);
    assert_eq!(outcome.code, 0);
    let leftovers = sandbox.leftover_tempdirs();
    assert_eq!(leftovers.len(), 1);
    let dir = &leftovers[0];
    let raw = dir.join("raw.jsonl");
    assert_eq!(
        std::fs::read_to_string(&raw).unwrap(),
        format!("{output}\n")
    );
    let executable = sandbox.bin().join("claude");
    assert_eq!(
        outcome.stderr,
        format!(
            "[debug] command: {} '<prompt 2 bytes>'\n[debug] PATH: {}\n[debug] set: (none)\n[debug] removed: (none)\n[debug] agentrun variables: (none)\n[debug] credentials: (none)\n[debug] tempdir: {}\n[debug] raw: {}\n",
            executable.display(),
            sandbox.path_var(),
            dir.display(),
            raw.display()
        )
    );
}

#[test]
fn executable_that_cannot_start_gives_single_failed_end() {
    let sandbox = Sandbox::new("#!/nonexistent/interpreter\n");
    let outcome = sandbox.run(&[], false);
    assert_eq!(outcome.code, 1);
    let events = outcome.events();
    assert_eq!(events.len(), 1);
    let end = &events[0];
    assert_eq!(end["type"], "end");
    assert_eq!(end["status"], "failed");
    assert_eq!(end["exit_code"], Value::Null);
    assert_eq!(end["usage"], empty_usage());
    let prefix = format!(
        "failed to start {}: ",
        sandbox.bin().join("claude").display()
    );
    assert!(
        end["detail"].as_str().unwrap().starts_with(&prefix),
        "{}",
        end["detail"]
    );
    assert!(sandbox.leftover_tempdirs().is_empty());
}

#[test]
fn dry_run_prints_quoted_command_and_path() {
    let sandbox = Sandbox::new("#!/bin/sh\ntouch \"$0.ran\"\n");
    let outcome = sandbox.run(&["--dry-run", "--", "--flag", "it's", "two words"], false);
    assert_eq!(outcome.code, 0);
    assert_eq!(
        outcome.stdout,
        format!(
            "command: {} --flag 'it'\\''s' 'two words' '<prompt 2 bytes>'\nPATH: {}\nset: (none)\nremoved: (none)\nagentrun variables: (none)\n",
            sandbox.bin().join("claude").display(),
            sandbox.path_var()
        )
    );
    assert!(outcome.stderr.is_empty());
    assert!(!sandbox.bin().join("claude.ran").exists());
    assert!(sandbox.leftover_tempdirs().is_empty());
}

#[test]
fn text_format_shows_note_and_lines() {
    let sandbox = Sandbox::with_output(concat!(
        r#"{"record":"subagent_start","id":"s1","parent":null,"kind":"Explore","model":"haiku","description":"look around"}"#,
        "\n",
        r#"{"record":"tool_start","id":"t1","parent":"s1","name":"Grep","summary":"Grep: foo"}"#,
        "\n",
        r#"{"record":"tool_end","id":"t1","denied":false}"#,
        "\n",
        r#"{"record":"usage","parent":"s1","model":"claude-haiku-4-5","input_tokens":30,"output_tokens":4,"cache_read_tokens":null,"cache_write_tokens":null}"#,
        "\n",
        r#"{"record":"subagent_end","id":"s1","status":"finished"}"#,
        "\n",
        r#"{"record":"text","parent":null,"text":"done\nmore"}"#
    ));
    let outcome = sandbox.run(&["--format", "text", "--sandbox", "relax"], false);
    assert_eq!(outcome.code, 0);
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert_eq!(
        lines[..4],
        [
            "[note] sandbox not running (--sandbox relax: not implemented in this build). agentrun does not restrict what the agent writes or which hosts it reaches",
            "[main] prompt hi",
            "[main] agent start explore#1 (haiku): look around",
            "[explore#1] tool Grep: foo",
        ]
    );
    assert!(lines[4].starts_with("[explore#1] agent finished "));
    assert!(lines[4].ends_with("s claude-haiku-4-5 in 30 out 4"));
    assert_eq!(lines[5], "[main] text done");
    assert!(lines[6].starts_with("[end] finished "));
    assert!(lines[6].ends_with("s in 30 out 4"));
    assert_eq!(lines.len(), 7);
}

#[test]
fn text_format_note_for_sandbox_off() {
    let sandbox = Sandbox::with_output("");
    let outcome = sandbox.run(&["--format=text"], false);
    assert_eq!(
        outcome.stdout.lines().next().unwrap(),
        "[note] sandbox not running (--sandbox off). agentrun does not restrict what the agent writes or which hosts it reaches"
    );
}

fn read_env_dump(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn child_environment_follows_assembly_rules() {
    let root = tempfile::tempdir().unwrap();
    let dump = root.path().join("env.txt");
    let sandbox = Sandbox::new(&format!("#!/bin/sh\n/usr/bin/env > '{}'\n", dump.display()));
    let env_file = sandbox.root.path().join("session.env");
    std::fs::write(
        &env_file,
        "FILE_VAR=from-file\nKEEP=file\nAGENTRUN_PI_AUTH=file-auth\n",
    )
    .unwrap();
    let bin = sandbox.bin().to_string_lossy().into_owned();
    let outcome = sandbox.run_as(
        "claude-code",
        &[
            ("ANTHROPIC_API_KEY", "caller-key"),
            ("CLAUDE_CODE_USE_VERTEX", "1"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "oauth"),
            ("KEEP", "caller"),
            ("INHERITED", "yes"),
            ("AGENTRUN_SANDBOX", "off"),
            ("AGENTRUN_UNKNOWN", "x"),
        ],
        &[
            "--env-file",
            env_file.to_str().unwrap(),
            "--env",
            "KEEP=arg",
            "--env",
            "ANTHROPIC_BASE_URL=https://example.test",
            "--env",
            "PATH=/usr/bin:/bin",
            "--env",
            "TMPDIR=/ignored",
            "--path",
            &bin,
        ],
        false,
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let child = read_env_dump(&dump);
    assert!(
        child.keys().all(|key| !key.starts_with("AGENTRUN_")),
        "{child:?}"
    );
    let session_path = format!("{bin}:/usr/bin:/bin");
    assert_eq!(child["PATH"], session_path);
    assert!(!child.contains_key("ANTHROPIC_API_KEY"));
    assert!(!child.contains_key("CLAUDE_CODE_USE_VERTEX"));
    assert_eq!(child["CLAUDE_CODE_OAUTH_TOKEN"], "oauth");
    assert_eq!(child["ANTHROPIC_BASE_URL"], "https://example.test");
    assert_eq!(child["KEEP"], "arg");
    assert_eq!(child["FILE_VAR"], "from-file");
    assert_eq!(child["INHERITED"], "yes");
    let tempdir = Path::new(&child["TMPDIR"]);
    assert_eq!(tempdir.parent().unwrap(), sandbox.tmp());
    assert_eq!(child["TMP"], child["TMPDIR"]);
    assert_eq!(child["TEMP"], child["TMPDIR"]);
    let start = &outcome.events()[0];
    assert_eq!(start["type"], "start");
    assert_eq!(
        start["env"],
        json!(["FILE_VAR", "KEEP", "ANTHROPIC_BASE_URL", "PATH", "TMPDIR"])
    );
}

#[test]
fn pi_credential_is_written_and_never_printed() {
    let secret = "pi-credential-value-7f3a9c";
    let sandbox = Sandbox::new("#!/bin/sh\nexit 0\n");
    sandbox.install(
        "pi",
        "#!/bin/sh\n/usr/bin/env\n/usr/bin/env >&2\nprintf '{\"record\":\"text\",\"parent\":null,\"text\":\"done\"}\\n'\n",
    );
    let home = sandbox.root.path().join("home");
    let home_str = home.to_string_lossy().into_owned();
    let raw = sandbox.root.path().join("raw.jsonl");
    let caller_env = [("HOME", home_str.as_str()), ("AGENTRUN_PI_AUTH", secret)];
    let args = ["--debug", "--raw", raw.to_str().unwrap()];
    let outcome = sandbox.run_as("pi", &caller_env, &args, false);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let login = home.join(".pi/agent/auth.json");
    assert_eq!(std::fs::read_to_string(&login).unwrap(), secret);
    assert!(home.join(".pi/agent/auth.json.agentrun-sha256").exists());
    assert!(outcome.stderr.contains(&format!(
        "[debug] credentials: AGENTRUN_PI_AUTH -> {} (written)\n",
        login.display()
    )));
    assert!(
        outcome
            .stderr
            .contains("[debug] agentrun variables: AGENTRUN_PI_AUTH\n")
    );
    assert!(outcome.stderr.contains(&format!("HOME={home_str}\n")));
    let raw_text = std::fs::read_to_string(&raw).unwrap();
    assert!(raw_text.contains(&format!("HOME={home_str}\n")));
    for text in [&outcome.stdout, &outcome.stderr, &raw_text] {
        assert!(!text.contains(secret));
    }
    std::fs::write(&login, "refreshed").unwrap();
    let again = sandbox.run_as("pi", &caller_env, &args, false);
    assert!(again.stderr.contains(&format!(
        "[debug] credentials: AGENTRUN_PI_AUTH -> {} (unchanged)\n",
        login.display()
    )));
    assert_eq!(std::fs::read_to_string(&login).unwrap(), "refreshed");
}

#[test]
fn dry_run_lists_variable_names_and_skips_credentials() {
    let sandbox = Sandbox::new("#!/bin/sh\nexit 0\n");
    sandbox.install("pi", "#!/bin/sh\ntouch \"$0.ran\"\n");
    let home = sandbox.root.path().join("home");
    let home_str = home.to_string_lossy().into_owned();
    let env_file = sandbox.root.path().join("a.env");
    std::fs::write(&env_file, "AGENTRUN_SANDBOX=relax\nZED=1\nALPHA=2\n").unwrap();
    let outcome = sandbox.run_as(
        "pi",
        &[
            ("HOME", home_str.as_str()),
            ("AGENTRUN_PI_AUTH", "secret"),
            ("GH_TOKEN", "token"),
        ],
        &[
            "--dry-run",
            "--env-file",
            env_file.to_str().unwrap(),
            "--env",
            "GH_TOKEN",
            "--env",
            "ZED=3",
        ],
        false,
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        format!(
            "command: {} '<prompt 2 bytes>'\nPATH: {}\nset: ZED, ALPHA, GH_TOKEN\nremoved: (none)\nagentrun variables: AGENTRUN_PI_AUTH, AGENTRUN_SANDBOX\n",
            sandbox.bin().join("pi").display(),
            sandbox.path_var()
        )
    );
    assert!(outcome.stderr.is_empty());
    assert!(!home.exists());
    assert!(!sandbox.bin().join("pi.ran").exists());
    assert!(sandbox.leftover_tempdirs().is_empty());
}

#[test]
fn debug_lists_removed_variables_for_claude_code() {
    let sandbox = Sandbox::with_output("");
    let outcome = sandbox.run_as(
        "claude-code",
        &[
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("ANTHROPIC_API_KEY", "key"),
            ("AGENTRUN_CODEX_AUTH", "codex"),
        ],
        &["--debug", "--env", "ANTHROPIC_API_KEY"],
        false,
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let lines: Vec<&str> = outcome.stderr.lines().collect();
    assert_eq!(
        lines[2..6],
        [
            "[debug] set: ANTHROPIC_API_KEY",
            "[debug] removed: ANTHROPIC_API_KEY, CLAUDE_CODE_USE_BEDROCK",
            "[debug] agentrun variables: AGENTRUN_CODEX_AUTH",
            "[debug] credentials: (none)",
        ]
    );
}

#[test]
fn credential_write_failure_is_rejected_before_tempdir() {
    let sandbox = Sandbox::new("#!/bin/sh\nexit 0\n");
    sandbox.install("pi", "#!/bin/sh\ntouch \"$0.ran\"\n");
    let blocker = sandbox.root.path().join("blocker");
    std::fs::write(&blocker, "").unwrap();
    let agent_dir = blocker.join("agent");
    let outcome = sandbox.run_as(
        "pi",
        &[
            ("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap()),
            ("AGENTRUN_PI_AUTH", "secret-value"),
        ],
        &[],
        false,
    );
    assert_eq!(outcome.code, 2);
    let detail = outcome.end()["detail"].as_str().unwrap().to_string();
    assert!(
        detail.starts_with(&format!(
            "cannot write AGENTRUN_PI_AUTH to {}: ",
            agent_dir.join("auth.json").display()
        )),
        "{detail}"
    );
    assert!(!outcome.stdout.contains("secret-value"));
    assert!(!outcome.stderr.contains("secret-value"));
    assert!(!sandbox.bin().join("pi.ran").exists());
    assert!(sandbox.leftover_tempdirs().is_empty());
}

#[test]
fn replay_fake_fixture() {
    support::assert_replay(
        &mut FakeAdapter::new(false),
        "fake",
        "basic",
        "fix the tests",
    );
}
