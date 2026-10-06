#[allow(dead_code)]
mod support;

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Instant;

use agentrun::pi::Pi;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use support::env::{Env, WAIT_LIMIT, poll_until};

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

fn user_state(env: &Env) -> PathBuf {
    env.home().join(".pi/agent")
}

fn write_user_file(env: &Env, name: &str, content: &str) -> PathBuf {
    let path = user_state(env).join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

fn command(env: &Env, args: &[&str]) -> Command {
    let work = env.work();
    env.command(&[&["pi", "--cwd", work.to_str().unwrap()], args].concat())
}

fn run(env: &Env, args: &[&str]) -> Output {
    command(env, args).output().unwrap()
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
    let env = Env::with("pi", FAKE_PI);
    let output = run(
        &env,
        &[
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
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = support::events(&output);
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
    assert!(env.leftovers().is_empty());
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
    let env = Env::with("pi", REPORTING_PI);
    let auth = write_user_file(
        &env,
        "auth.json",
        "{\"deepseek\":{\"type\":\"api_key\",\"key\":\"k\"}}",
    );
    let fd = write_user_file(&env, "bin/fd", "#!/bin/sh\necho fd\n");
    let settings_text = "{\"defaultProvider\":\"deepseek\",\"defaultModel\":\"deepseek-flash\",\"packages\":[\"x\"],\"theme\":\"dark\"}";
    let settings = write_user_file(&env, "settings.json", settings_text);
    let output = run(&env, &["--sandbox", "off", "--prompt", "hi"]);
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
    let names: Vec<String> = std::fs::read_dir(user_state(&env))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 3);
    assert!(env.leftovers().is_empty());
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["status"], "finished");
    assert_eq!(end["result"], "ok");
}

#[test]
fn missing_user_state_dir_is_created_before_pi_starts() {
    let env = Env::with("pi", REPORTING_PI);
    let output = run(&env, &["--sandbox", "off", "--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report = std::fs::read_to_string(env.work().join("state.txt")).unwrap();
    assert!(report.contains("\nentries=\n"), "{report}");
    let mode = |path: &PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&env.home().join(".pi")), 0o700);
    assert_eq!(mode(&user_state(&env)), 0o700);
}

#[test]
fn model_mismatch_terminates_the_run() {
    let env = Env::with(
        "pi",
        concat!(
            "#!/bin/sh\n",
            "echo '{\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"content\":[],\"provider\":\"amazon-bedrock\",\"model\":\"nova\",\"stopReason\":\"pending\"}}'\n",
            "sleep 30\n",
        ),
    );
    let started = Instant::now();
    let output = run(
        &env,
        &[
            "--sandbox",
            "off",
            "--model",
            "deepseek/deepseek-flash",
            "--prompt",
            "hi",
        ],
    );
    assert!(
        started.elapsed().as_secs_f64() < 10.0,
        "{:?}",
        started.elapsed()
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["status"], "failed");
    assert_eq!(
        end["detail"],
        "pi uses amazon-bedrock/nova, which does not match --model deepseek/deepseek-flash"
    );
    assert_eq!(end["exit_code"], Value::Null);
    assert!(env.leftovers().is_empty());
}

#[test]
fn model_without_provider_is_a_usage_error() {
    let env = Env::with("pi", FAKE_PI);
    let output = run(
        &env,
        &[
            "--sandbox",
            "off",
            "--model",
            "deepseek-flash",
            "--prompt",
            "hi",
        ],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["status"], "rejected");
    assert_eq!(
        end["detail"],
        "--model for pi must be provider/model, got 'deepseek-flash'"
    );
    assert!(env.leftovers().is_empty());
}

#[test]
fn exit_without_reply_fails() {
    let env = Env::with("pi", "#!/bin/sh\nexit 0\n");
    let output = run(&env, &["--sandbox", "off", "--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["status"], "failed");
    assert_eq!(end["detail"], "pi produced no model reply");
}

#[test]
fn dry_run_creates_no_state_dir_and_lists_no_state_variable() {
    let env = Env::with("pi", FAKE_PI);
    write_user_file(&env, "auth.json", "{}");
    let output = run(
        &env,
        &[
            "--sandbox",
            "off",
            "--dry-run",
            "--prompt",
            "hi",
            "--",
            "-x",
        ],
    );
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
    assert!(env.leftovers().is_empty());
}

const CURL_PI: &str = r#"#!/bin/sh
port=$(cat "$PWD/port")
code=$(curl -sS --retry-connrefused --retry 5 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/" 2>/dev/null || echo failed)
printf '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"code=%s proxy=%s https=%s all=%s noproxy=%s/%s"}],"provider":"deepseek","model":"deepseek-flash","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0},"stopReason":"stop"}}\n' \
  "$code" "$http_proxy" "$HTTPS_PROXY" "$ALL_PROXY" "${NO_PROXY-unset}" "${no_proxy-unset}"
"#;

#[test]
fn sandboxed_pi_reaches_the_web_only_through_the_filter_proxy() {
    if !support::sandbox_available() {
        return;
    }
    let server = support::WebServer::start();
    let env = Env::with("pi", CURL_PI);
    std::fs::write(env.work().join("port"), server.port.to_string()).unwrap();
    let model = ["--model", "deepseek/deepseek-flash", "--prompt", "fetch"];

    let mut args = vec!["--network", "none"];
    args.extend(model);
    let output = run(&env, &args);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let first = support::events(&output);
    assert_eq!(
        first[0]["network"],
        json!({"mode": "none", "allow": [], "enforced": true})
    );
    assert_eq!(first[0]["env"], json!([]));
    let text = support::text_of(&first, 0);
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
        support::network_events(&first),
        [
            json!({"schema": 1, "type": "network", "host": "127.0.0.1", "port": server.port, "allowed": false, "reason": "not_allowed"})
        ]
    );

    let port_rule = format!("127.0.0.1:{}", server.port);
    let mut args = vec!["--network", "custom", "--allow-host", &port_rule];
    args.extend(model);
    let output = run(&env, &args);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let second = support::events(&output);
    assert_eq!(
        second[0]["network"],
        json!({"mode": "custom", "allow": [port_rule], "enforced": true})
    );
    assert!(
        support::text_of(&second, 0).starts_with("code=200 "),
        "{}",
        support::text_of(&second, 0)
    );
    assert_eq!(
        support::network_events(&second),
        [
            json!({"schema": 1, "type": "network", "host": "127.0.0.1", "port": server.port, "allowed": true, "reason": null})
        ]
    );

    let mut args = vec!["--format", "text", "--network", "full"];
    args.extend(model);
    let output = run(&env, &args);
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
    assert!(env.leftovers().is_empty());
}

#[test]
fn sandboxed_pi_with_unknown_provider_needs_an_allowed_host() {
    if !support::sandbox_available() {
        return;
    }
    let env = Env::with("pi", CURL_PI);
    write_user_file(&env, "settings.json", "{\"defaultProvider\":\"acme\"}");
    let output = run(&env, &["--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["status"], "rejected");
    assert_eq!(
        end["detail"],
        "cannot tell which host pi's model service uses (provider: acme). Use --network custom --allow-host <host of the model service>"
    );
    assert!(env.leftovers().is_empty());
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
    let env = Env::with("pi", SOCKET_REMOVING_PI);
    let mut child = command(
        &env,
        &["--model", "deepseek/deepseek-flash", "--prompt", "hi"],
    )
    .spawn()
    .unwrap();
    if !poll_until(WAIT_LIMIT, || child.try_wait().unwrap().is_some()) {
        let _ = child.kill();
        panic!("agentrun did not end after the proxy socket was removed");
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(env.work().join("state.txt")).unwrap(),
        "removed=yes\n"
    );
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["type"], "end");
    assert_eq!(end["status"], "finished");
    assert!(env.leftovers().is_empty());
}
