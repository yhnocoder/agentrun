#[allow(dead_code)]
mod support;

use std::io::Write;
use std::process::{Output, Stdio};

use agentrun::claudecode::ClaudeCode;
use serde_json::{Value, json};
use support::env::Env;

fn replay(name: &str) {
    support::assert_replay(&mut ClaudeCode::new(), "claude-code", name, "unused");
}

#[test]
fn replay_basic_task() {
    replay("basic-task");
}

#[test]
fn replay_basic_task_auto() {
    replay("basic-task-auto");
}

#[test]
fn replay_subagents() {
    replay("subagents");
}

#[test]
fn replay_no_subagents() {
    replay("no-subagents");
}

#[test]
fn replay_outside_write() {
    replay("outside-write");
}

#[test]
fn replay_write_tool_outside() {
    replay("write-tool-outside");
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

#[test]
fn replay_sigint() {
    replay("sigint");
}

#[test]
fn replay_prompt_multiline() {
    replay("prompt-multiline");
}

fn with_claude(script: &str) -> Env {
    let env = Env::new();
    env.install("claude", script);
    env
}

fn run(env: &Env, args: &[&str], stdin: &str) -> Output {
    let work = env.work();
    let mut child = env
        .command(&[&["claude-code", "--cwd", work.to_str().unwrap()], args].concat())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

const FAKE_CLAUDE: &str = r#"#!/bin/sh
read -r line
printf '%s' "$line" > "$0.stdin"
cat <<'LINES'
{"type":"system","subtype":"init","tools":["Task","Bash","Edit","Glob","Grep","Read","Write"],"permissionMode":"auto"}
{"type":"user","isReplay":true,"message":{"role":"user","content":"say hi"},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"msg_1","model":"claude-sonnet-5-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"echo hi\necho there"}}],"usage":{"input_tokens":1,"output_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}},"parent_tool_use_id":null}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"hi"}]},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"msg_2","model":"claude-sonnet-5-5","content":[{"type":"text","text":"hi there"}],"usage":{"input_tokens":5,"output_tokens":6,"cache_read_input_tokens":7,"cache_creation_input_tokens":8}},"parent_tool_use_id":null}
{"type":"result","subtype":"success","is_error":false,"result":"hi there","modelUsage":{"claude-sonnet-5-5":{"inputTokens":6,"outputTokens":8,"cacheReadInputTokens":10,"cacheCreationInputTokens":12}}}
LINES
"#;

#[test]
fn fake_claude_run_without_sandbox() {
    let env = with_claude(FAKE_CLAUDE);
    let output = run(
        &env,
        &["--sandbox", "off", "--model", "sonnet", "--", "--extra"],
        "say hi\n",
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(env.bin().join("claude.stdin")).unwrap(),
        r#"{"type":"user","message":{"role":"user","content":"say hi\n"}}"#
    );
    let events = support::events(&output);
    let start = &events[0];
    let argv: Vec<&str> = start["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap())
        .collect();
    let session_id = argv[15];
    assert_eq!(session_id.len(), 36);
    assert_eq!(&session_id[14..15], "4");
    let executable = env.bin().join("claude").to_string_lossy().into_owned();
    assert_eq!(
        argv,
        [
            executable.as_str(),
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--replay-user-messages",
            "--permission-mode",
            "auto",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--session-id",
            session_id,
            "--tools",
            "Read,Edit,Write,Glob,Grep,Bash,Task",
            "--allowedTools",
            "Read,Edit,Write,Glob,Grep,Task",
            "--model",
            "sonnet",
            "--extra",
        ]
    );
    assert_eq!(
        events[1..],
        [
            json!({"schema": 1, "type": "prompt", "text": "say hi"}),
            json!({"schema": 1, "type": "tool", "id": "toolu_1", "parent": null, "name": "Bash", "summary": "Bash: echo hi", "denied": false}),
            json!({"schema": 1, "type": "usage", "parent": null, "model": "claude-sonnet-5-5", "input_tokens": 1, "output_tokens": 2,
                "cache_read_tokens": 3, "cache_write_tokens": 4, "context_tokens": 8}),
            json!({"schema": 1, "type": "text", "parent": null, "text": "hi there"}),
            json!({"schema": 1, "type": "usage", "parent": null, "model": "claude-sonnet-5-5", "input_tokens": 5, "output_tokens": 6,
                "cache_read_tokens": 7, "cache_write_tokens": 8, "context_tokens": 20}),
            json!({"schema": 1, "type": "end", "status": "finished", "exit_code": 0, "detail": "",
                "usage": {"input_tokens": 6, "output_tokens": 8, "cache_read_tokens": 10, "cache_write_tokens": 12,
                    "by_model": {"claude-sonnet-5-5": {"input_tokens": 6, "output_tokens": 8, "cache_read_tokens": 10, "cache_write_tokens": 12}}},
                "result": "hi there"}),
        ]
    );
}

#[test]
fn no_subagents_removes_task_and_reports_when_it_comes_back() {
    let env = with_claude(FAKE_CLAUDE);
    let output = run(
        &env,
        &[
            "--sandbox",
            "off",
            "--no-subagents",
            "--debug",
            "--prompt",
            "say hi",
        ],
        "",
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let start = &support::events(&output)[0];
    let argv = start["argv"].as_array().unwrap();
    assert_eq!(argv[17], "Read,Edit,Write,Glob,Grep,Bash");
    assert_eq!(argv[19], "Read,Edit,Write,Glob,Grep");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("[debug] subagents: Task is in the tool list despite --no-subagents\n"),
        "{stderr}"
    );
    let output = run(
        &env,
        &["--sandbox", "off", "--debug", "--prompt", "say hi"],
        "",
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("[debug] subagents"), "{stderr}");
}

#[test]
fn exit_without_result_fails() {
    let env = with_claude("#!/bin/sh\nexit 0\n");
    let output = run(&env, &["--sandbox", "off", "--prompt", "hi"], "");
    assert_eq!(output.status.code(), Some(1));
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["status"], "failed");
    assert_eq!(
        end["detail"],
        "runtime exited with code 0 without a result event"
    );
}

#[test]
fn dry_run_with_sandbox_writes_tempdir_placeholder() {
    if !support::sandbox_available() {
        return;
    }
    let env = with_claude(FAKE_CLAUDE);
    let output = run(&env, &["--dry-run", "--prompt", "hi"], "");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let command = stdout.lines().next().unwrap();
    let work = env.work().to_string_lossy().into_owned();
    assert!(
        command.contains(&format!(
            "--allowedTools 'Read,Glob,Grep,Edit(/{work}/**),Write(/{work}/**),Edit(//<tempdir>/**),Write(//<tempdir>/**),Task'"
        )),
        "{command}"
    );
    assert!(
        command.contains(&format!(
            r#"--settings '{{"sandbox":{{"enabled":true,"failIfUnavailable":true,"autoAllowBashIfSandboxed":true,"allowUnsandboxedCommands":false,"filesystem":{{"allowWrite":["{work}","<tempdir>"]}},"network":{{"allowedDomains":[]}}}}}}'"#
        )),
        "{command}"
    );
    assert!(command.contains("--permission-mode dontAsk"), "{command}");
    assert!(env.leftovers().is_empty());
}

const CURL_CLAUDE: &str = r#"#!/bin/sh
read -r line
prev=""
settings=""
for arg in "$@"; do
  if [ "$prev" = "--settings" ]; then settings="$arg"; fi
  prev="$arg"
done
port=$(printf '%s' "$settings" | sed -n 's/.*"httpProxyPort":\([0-9]*\).*/\1/p')
if [ -n "$port" ]; then
  code=$(curl -sS -x "http://127.0.0.1:$port" -o /dev/null -w '%{http_code}' "http://127.0.0.1:$(cat "$PWD/port")/" 2>/dev/null || echo failed)
else
  code=noproxy
fi
cat <<LINES
{"type":"system","subtype":"init","tools":["Bash"],"permissionMode":"dontAsk"}
{"type":"user","isReplay":true,"message":{"role":"user","content":"fetch"},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"msg_1","model":"claude-sonnet-5-5","content":[{"type":"text","text":"code=$code socat=${SOCAT_DEFAULT_LISTEN_IP-unset} proxy=${HTTPS_PROXY-unset}"}],"usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"parent_tool_use_id":null}
{"type":"result","subtype":"success","is_error":false,"result":"done","modelUsage":{}}
LINES
"#;

fn option_value(argv: &Value, name: &str) -> String {
    let argv = argv.as_array().unwrap();
    let index = argv.iter().position(|arg| arg == name).unwrap();
    argv[index + 1].as_str().unwrap().to_string()
}

fn network_settings(argv: &Value) -> Value {
    let settings: Value = serde_json::from_str(&option_value(argv, "--settings")).unwrap();
    settings["sandbox"]["network"].clone()
}

#[test]
fn sandboxed_claude_gets_the_filter_proxy_ports_in_full_and_custom() {
    if !support::sandbox_available() {
        return;
    }
    let server = support::WebServer::start();
    let env = with_claude(CURL_CLAUDE);
    std::fs::write(env.work().join("port"), server.port.to_string()).unwrap();

    let output = run(&env, &["--network", "full", "--prompt", "fetch"], "");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let full = support::events(&output);
    let start = &full[0];
    assert_eq!(
        start["network"],
        json!({"mode": "full", "allow": [], "enforced": true})
    );
    assert_eq!(start["env"], json!([]));
    let network = network_settings(&start["argv"]);
    let port = network["httpProxyPort"].as_u64().unwrap();
    assert!(port > 0);
    assert_eq!(
        network,
        json!({"allowedDomains": [], "httpProxyPort": port, "socksProxyPort": port})
    );
    assert_eq!(
        option_value(&start["argv"], "--tools"),
        "Read,Edit,Write,Glob,Grep,Bash,Task,WebFetch,WebSearch"
    );
    assert!(
        option_value(&start["argv"], "--allowedTools").ends_with(",Task,WebFetch,WebSearch"),
        "{}",
        option_value(&start["argv"], "--allowedTools")
    );
    assert_eq!(support::text_of(&full, 0), "code=403 socat=4 proxy=unset");
    assert_eq!(
        support::network_events(&full),
        [
            json!({"schema": 1, "type": "network", "host": "127.0.0.1", "port": server.port, "allowed": false, "reason": "private_address"})
        ]
    );

    let port_rule = format!("127.0.0.1:{}", server.port);
    let output = run(
        &env,
        &[
            "--network",
            "custom",
            "--allow-host",
            &port_rule,
            "--allow-host",
            "*.example.com:8443",
            "--prompt",
            "fetch",
        ],
        "",
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let custom = support::events(&output);
    let start = &custom[0];
    assert_eq!(
        start["network"],
        json!({"mode": "custom", "allow": [port_rule, "*.example.com:8443"], "enforced": true})
    );
    let network = network_settings(&start["argv"]);
    assert!(network["httpProxyPort"].is_u64(), "{network}");
    assert_eq!(network["httpProxyPort"], network["socksProxyPort"]);
    assert_eq!(
        option_value(&start["argv"], "--tools"),
        "Read,Edit,Write,Glob,Grep,Bash,Task,WebFetch"
    );
    assert!(
        option_value(&start["argv"], "--allowedTools")
            .ends_with(",Task,WebFetch(domain:127.0.0.1),WebFetch(domain:*.example.com)"),
        "{}",
        option_value(&start["argv"], "--allowedTools")
    );
    assert_eq!(support::text_of(&custom, 0), "code=200 socat=4 proxy=unset");
    assert_eq!(
        support::network_events(&custom),
        [
            json!({"schema": 1, "type": "network", "host": "127.0.0.1", "port": server.port, "allowed": true, "reason": null})
        ]
    );

    let output = run(&env, &["--prompt", "fetch"], "");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let none = support::events(&output);
    let start = &none[0];
    assert_eq!(
        start["network"],
        json!({"mode": "none", "allow": [], "enforced": true})
    );
    assert_eq!(
        network_settings(&start["argv"]),
        json!({"allowedDomains": []})
    );
    assert_eq!(
        option_value(&start["argv"], "--tools"),
        "Read,Edit,Write,Glob,Grep,Bash,Task"
    );
    assert_eq!(
        support::text_of(&none, 0),
        "code=noproxy socat=4 proxy=unset"
    );
    assert!(support::network_events(&none).is_empty());

    let output = run(
        &env,
        &["--sandbox", "off", "--network", "full", "--prompt", "fetch"],
        "",
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let open = support::events(&output);
    let start = &open[0];
    assert_eq!(
        start["network"],
        json!({"mode": "full", "allow": [], "enforced": false})
    );
    assert!(
        !start["argv"]
            .as_array()
            .unwrap()
            .contains(&json!("--settings"))
    );
    assert_eq!(
        option_value(&start["argv"], "--allowedTools"),
        "Read,Edit,Write,Glob,Grep,Task,WebFetch,WebSearch"
    );
    assert_eq!(
        support::text_of(&open, 0),
        "code=noproxy socat=unset proxy=unset"
    );
    assert_eq!(server.served(), 1);
    assert!(env.leftovers().is_empty());
}
