#[allow(dead_code)]
mod support;

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Instant;

use agentrun::pi::Pi;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn replay(name: &str) {
    support::assert_replay(&mut Pi::new(), "pi", name, "unused");
}

#[test]
fn replay_basic_task() {
    replay("basic-task");
}

#[test]
fn replay_outside_write() {
    replay("outside-write");
}

#[test]
fn replay_network() {
    replay("network");
}

#[test]
fn replay_config_isolation() {
    replay("config-isolation");
}

#[test]
fn replay_model_error() {
    replay("model-error");
}

#[test]
fn replay_cred_error() {
    replay("cred-error");
}

#[test]
fn replay_arg_error() {
    replay("arg-error");
}

struct Env {
    root: TempDir,
}

impl Env {
    fn new(script: &str) -> Env {
        let env = Env {
            root: tempfile::tempdir().unwrap(),
        };
        for dir in [env.bin(), env.tmp(), env.work(), env.home()] {
            std::fs::create_dir(dir).unwrap();
        }
        let path = env.bin().join("pi");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        env
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

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn user_state(&self) -> PathBuf {
        self.home().join(".pi/agent")
    }

    fn write_user_file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.user_state().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agentrun"))
            .arg("pi")
            .arg("--cwd")
            .arg(self.work())
            .args(args)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            .env("TMPDIR", self.tmp())
            .env("HOME", self.home())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn leftover_tempdirs(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.tmp())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }
}

fn events(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| support::without_timing(serde_json::from_str(line).unwrap()))
        .collect()
}

const FAKE_PI: &str = r#"#!/bin/sh
cat <<'LINES'
{"type":"session","version":3,"id":"0199-fake","timestamp":"2026-10-04T00:00:00.000Z","cwd":"/work"}
{"type":"agent_start"}
{"type":"turn_start"}
{"type":"message_start","message":{"role":"system","content":"","sections":{}}}
{"type":"message_end","message":{"role":"system","content":"","sections":{}}}
{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"say hi"}]}}
{"type":"message_start","message":{"role":"assistant","content":[],"provider":"deepseek","model":"deepseek-flash","stopReason":"pending"}}
{"type":"message_update","message":{"role":"assistant","content":[]}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"run it"},{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"echo hi\necho there"}}],"provider":"deepseek","model":"deepseek-flash","usage":{"input":1,"output":2,"cacheRead":3,"cacheWrite":4,"reasoning":0,"totalTokens":10,"cost":{"total":0}},"stopReason":"toolUse"}}
{"type":"tool_execution_start","toolCallId":"call_1","toolName":"bash","args":{"command":"echo hi\necho there","timeout":30}}
{"type":"tool_execution_end","toolCallId":"call_1","toolName":"bash","result":{"content":[{"type":"text","text":"hi\nthere\n"}]},"isError":false}
{"type":"message_end","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{"type":"text","text":"hi\nthere\n"}],"isError":false}}
{"type":"turn_end"}
{"type":"turn_start"}
{"type":"message_start","message":{"role":"assistant","content":[],"provider":"deepseek","model":"deepseek-flash","stopReason":"pending"}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"hi there"}],"provider":"deepseek","model":"deepseek-flash","usage":{"input":5,"output":6,"cacheRead":7,"cacheWrite":8,"reasoning":0,"totalTokens":26,"cost":{"total":0}},"stopReason":"stop"}}
{"type":"turn_end"}
{"type":"agent_end","messages":[]}
{"type":"agent_settled"}
LINES
"#;

#[test]
fn fake_pi_run_without_sandbox() {
    let env = Env::new(FAKE_PI);
    let output = env.run(&[
        "--sandbox",
        "off",
        "--model",
        "deepseek/deepseek-flash:high",
        "--effort",
        "low",
        "--prompt",
        "say hi",
        "--",
        "--verbose",
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = events(&output);
    let start = &events[0];
    let executable = env.bin().join("pi").to_string_lossy().into_owned();
    assert_eq!(
        start["argv"],
        json!([
            executable,
            "-p",
            "--mode",
            "json",
            "--no-session",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--no-approve",
            "--offline",
            "--tools",
            "read,bash,edit,write,grep,find,ls",
            "--model",
            "deepseek/deepseek-flash:high",
            "--thinking",
            "low",
            "--verbose",
            "--",
        ])
    );
    assert_eq!(start["model"], "deepseek/deepseek-flash:high");
    assert_eq!(start["env"], json!([]));
    assert_eq!(
        events[1..],
        [
            json!({"schema": 1, "type": "prompt", "text": "say hi"}),
            json!({"schema": 1, "type": "usage", "parent": null, "model": "deepseek/deepseek-flash", "input_tokens": 1, "output_tokens": 2,
                "cache_read_tokens": 3, "cache_write_tokens": 4, "context_tokens": 8}),
            json!({"schema": 1, "type": "tool", "id": "call_1", "parent": null, "name": "bash", "summary": "bash: echo hi", "denied": false}),
            json!({"schema": 1, "type": "text", "parent": null, "text": "hi there"}),
            json!({"schema": 1, "type": "usage", "parent": null, "model": "deepseek/deepseek-flash", "input_tokens": 5, "output_tokens": 6,
                "cache_read_tokens": 7, "cache_write_tokens": 8, "context_tokens": 20}),
            json!({"schema": 1, "type": "end", "status": "finished", "exit_code": 0, "detail": "",
                "usage": {"input_tokens": 6, "output_tokens": 8, "cache_read_tokens": 10, "cache_write_tokens": 12,
                    "by_model": {"deepseek/deepseek-flash": {"input_tokens": 6, "output_tokens": 8, "cache_read_tokens": 10, "cache_write_tokens": 12}}},
                "result": "hi there"}),
        ]
    );
    assert!(env.leftover_tempdirs().is_empty());
}

const REPORTING_PI: &str = r#"#!/bin/sh
{
  echo "dir=${PI_CODING_AGENT_DIR-unset}"
  echo "tmpdir=$TMPDIR"
  echo "entries=$(ls -A "$HOME/.pi/agent" | sort | tr '\n' ' ')"
  echo "tmpentries=$(ls -A "$TMPDIR" | sort | tr '\n' ' ')"
} > "$PWD/state.txt"
echo '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"ok"}],"provider":"deepseek","model":"deepseek-flash","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0},"stopReason":"stop"}}'
"#;

#[test]
fn pi_uses_the_user_state_dir_which_stays_unchanged() {
    let env = Env::new(REPORTING_PI);
    let auth = env.write_user_file(
        "auth.json",
        "{\"deepseek\":{\"type\":\"api_key\",\"key\":\"k\"}}",
    );
    let fd = env.write_user_file("bin/fd", "#!/bin/sh\necho fd\n");
    let settings_text = "{\"defaultProvider\":\"deepseek\",\"defaultModel\":\"deepseek-flash\",\"packages\":[\"x\"],\"theme\":\"dark\"}";
    let settings = env.write_user_file("settings.json", settings_text);
    let output = env.run(&["--sandbox", "off", "--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report = std::fs::read_to_string(env.work().join("state.txt")).unwrap();
    let field = |name: &str| {
        report
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("{name} in {report}"))
            .to_string()
    };
    assert_eq!(field("dir"), "unset");
    assert_eq!(PathBuf::from(field("tmpdir")).parent().unwrap(), env.tmp());
    assert_eq!(field("entries"), "auth.json bin settings.json ");
    assert_eq!(field("tmpentries"), "");
    assert_eq!(
        std::fs::read_to_string(&auth).unwrap(),
        "{\"deepseek\":{\"type\":\"api_key\",\"key\":\"k\"}}"
    );
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), settings_text);
    assert!(fd.exists());
    let names: Vec<String> = std::fs::read_dir(env.user_state())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 3);
    assert!(env.leftover_tempdirs().is_empty());
    let end = events(&output).pop().unwrap();
    assert_eq!(end["status"], "finished");
    assert_eq!(end["result"], "ok");
}

#[test]
fn missing_user_state_dir_is_created_before_pi_starts() {
    let env = Env::new(REPORTING_PI);
    let output = env.run(&["--sandbox", "off", "--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report = std::fs::read_to_string(env.work().join("state.txt")).unwrap();
    assert!(report.contains("\nentries=\n"), "{report}");
    let mode = |path: &PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&env.home().join(".pi")), 0o700);
    assert_eq!(mode(&env.user_state()), 0o700);
}

#[test]
fn model_mismatch_terminates_the_run() {
    let env = Env::new(concat!(
        "#!/bin/sh\n",
        "echo '{\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"content\":[],\"provider\":\"amazon-bedrock\",\"model\":\"nova\",\"stopReason\":\"pending\"}}'\n",
        "sleep 30\n",
    ));
    let started = Instant::now();
    let output = env.run(&[
        "--sandbox",
        "off",
        "--model",
        "deepseek/deepseek-flash",
        "--prompt",
        "hi",
    ]);
    assert!(
        started.elapsed().as_secs_f64() < 2.0,
        "{:?}",
        started.elapsed()
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let end = events(&output).pop().unwrap();
    assert_eq!(end["status"], "failed");
    assert_eq!(
        end["detail"],
        "pi uses amazon-bedrock/nova, which does not match --model deepseek/deepseek-flash"
    );
    assert_eq!(end["exit_code"], Value::Null);
    assert!(env.leftover_tempdirs().is_empty());
}

#[test]
fn model_without_provider_is_a_usage_error() {
    let env = Env::new(FAKE_PI);
    let output = env.run(&[
        "--sandbox",
        "off",
        "--model",
        "deepseek-flash",
        "--prompt",
        "hi",
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let end = events(&output).pop().unwrap();
    assert_eq!(end["status"], "rejected");
    assert_eq!(
        end["detail"],
        "--model for pi must be provider/model, got 'deepseek-flash'"
    );
    assert!(env.leftover_tempdirs().is_empty());
}

#[test]
fn exit_without_reply_fails() {
    let env = Env::new("#!/bin/sh\nexit 0\n");
    let output = env.run(&["--sandbox", "off", "--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let end = events(&output).pop().unwrap();
    assert_eq!(end["status"], "failed");
    assert_eq!(end["detail"], "pi produced no model reply");
}

#[test]
fn dry_run_creates_no_state_dir_and_lists_no_state_variable() {
    let env = Env::new(FAKE_PI);
    env.write_user_file("auth.json", "{}");
    let output = env.run(&[
        "--sandbox",
        "off",
        "--dry-run",
        "--prompt",
        "hi",
        "--",
        "-x",
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines[0].ends_with(" --tools read,bash,edit,write,grep,find,ls -x -- '<prompt 2 bytes>'"),
        "{}",
        lines[0]
    );
    assert_eq!(lines[2], "set: (none)");
    assert!(!stdout.contains("PI_CODING_AGENT_DIR"), "{stdout}");
    assert!(env.leftover_tempdirs().is_empty());
}

const CURL_PI: &str = r#"#!/bin/sh
port=$(cat "$PWD/port")
code=$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/" 2>/dev/null || echo failed)
printf '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"code=%s proxy=%s https=%s all=%s noproxy=%s/%s"}],"provider":"deepseek","model":"deepseek-flash","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0},"stopReason":"stop"}}\n' \
  "$code" "$http_proxy" "$HTTPS_PROXY" "$ALL_PROXY" "${NO_PROXY-unset}" "${no_proxy-unset}"
"#;

fn text_of(events: &[Value]) -> String {
    events
        .iter()
        .find(|event| event["type"] == "text")
        .map(|event| event["text"].as_str().unwrap().to_string())
        .unwrap_or_else(|| panic!("no text event in {events:?}"))
}

fn network_events(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event["type"] == "network")
        .cloned()
        .collect()
}

#[test]
fn sandboxed_pi_reaches_the_web_only_through_the_filter_proxy() {
    if !support::sandbox_available() {
        return;
    }
    let server = support::WebServer::start();
    let env = Env::new(CURL_PI);
    std::fs::write(env.work().join("port"), server.port.to_string()).unwrap();
    let model = ["--model", "deepseek/deepseek-flash", "--prompt", "fetch"];

    let mut args = vec!["--network", "none"];
    args.extend(model);
    let output = env.run(&args);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let first = events(&output);
    assert_eq!(
        first[0]["network"],
        json!({"mode": "none", "allow": [], "enforced": true})
    );
    assert_eq!(first[0]["env"], json!([]));
    let text = text_of(&first);
    assert!(
        text.starts_with("code=403 proxy=http://127.0.0.1:"),
        "{text}"
    );
    let proxy = text.split(' ').nth(1).unwrap();
    let address = proxy.strip_prefix("proxy=").unwrap();
    assert_eq!(
        text,
        format!("code=403 proxy={address} https={address} all={address} noproxy=/")
    );
    assert_eq!(
        network_events(&first),
        [
            json!({"schema": 1, "type": "network", "host": "127.0.0.1", "port": server.port, "allowed": false, "reason": "not_allowed"})
        ]
    );

    let port_rule = format!("127.0.0.1:{}", server.port);
    let mut args = vec!["--network", "custom", "--allow-host", &port_rule];
    args.extend(model);
    let output = env.run(&args);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let second = events(&output);
    assert_eq!(
        second[0]["network"],
        json!({"mode": "custom", "allow": [port_rule], "enforced": true})
    );
    assert!(
        text_of(&second).starts_with("code=200 "),
        "{}",
        text_of(&second)
    );
    assert_eq!(
        network_events(&second),
        [
            json!({"schema": 1, "type": "network", "host": "127.0.0.1", "port": server.port, "allowed": true, "reason": null})
        ]
    );

    let mut args = vec!["--format", "text", "--network", "full"];
    args.extend(model);
    let output = env.run(&args);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(&format!(
            "[main] net denied 127.0.0.1:{} (private_address)\n",
            server.port
        )),
        "{stdout}"
    );
    assert!(stdout.contains("code=403 "), "{stdout}");
    assert_eq!(server.served(), 1);
    assert!(env.leftover_tempdirs().is_empty());
}

#[test]
fn sandboxed_pi_with_unknown_provider_needs_an_allowed_host() {
    if !support::sandbox_available() {
        return;
    }
    let env = Env::new(CURL_PI);
    env.write_user_file("settings.json", "{\"defaultProvider\":\"acme\"}");
    let output = env.run(&["--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let end = events(&output).pop().unwrap();
    assert_eq!(end["status"], "rejected");
    assert_eq!(
        end["detail"],
        "cannot tell which host pi's model service uses (provider: acme). Use --network custom --allow-host <host of the model service>"
    );
    assert!(env.leftover_tempdirs().is_empty());
}

const SOCKET_REMOVING_PI: &str = r#"#!/bin/sh
rm -f "$TMPDIR/proxy.sock"
echo "removed=$(test -e "$TMPDIR/proxy.sock" && echo no || echo yes)" > "$PWD/state.txt"
echo '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"ok"}],"provider":"deepseek","model":"deepseek-flash","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0},"stopReason":"stop"}}'
"#;

#[test]
fn run_ends_after_the_sandboxed_agent_removes_the_proxy_socket() {
    if !support::sandbox_available() {
        return;
    }
    let env = Env::new(SOCKET_REMOVING_PI);
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentrun"))
        .arg("pi")
        .arg("--cwd")
        .arg(env.work())
        .args(["--model", "deepseek/deepseek-flash", "--prompt", "hi"])
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", env.bin().display()))
        .env("TMPDIR", env.tmp())
        .env("HOME", env.home())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("agentrun did not end after the proxy socket was removed");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(env.work().join("state.txt")).unwrap(),
        "removed=yes\n"
    );
    let end = events(&output).pop().unwrap();
    assert_eq!(end["type"], "end");
    assert_eq!(end["status"], "finished");
    assert!(env.leftover_tempdirs().is_empty());
}
