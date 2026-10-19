#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agentrun::runtime::dsh::Dsh;
use serde_json::{Value, json};
use support::env::Env;

const KEY: &str = "sk-test";
const SESSION_ID: &str = "bd2ebf55-0917-4915-8793-20a5fc640cb9";

fn replay(name: &str) {
    let events = read(support::fixture("dsh", name, "events.jsonl"));
    let first: Value = serde_json::from_str(events.lines().next().unwrap()).unwrap();
    assert_eq!(first["type"], "prompt");
    support::assert_replay(
        &mut Dsh::new(),
        "dsh",
        name,
        first["text"].as_str().unwrap(),
    );
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
fn replay_no_subagents() {
    replay("no-subagents");
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

fn command(env: &Env, args: &[&str]) -> Command {
    let work = env.work();
    let mut command = env.command(&[&["dsh", "--cwd", work.to_str().unwrap()], args].concat());
    command.env("DEEPSEEK_API_KEY", KEY);
    command
}

fn run(env: &Env, args: &[&str]) -> Output {
    command(env, args).output().unwrap()
}

fn read(path: PathBuf) -> String {
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn project_key(cwd: &Path) -> String {
    let text = cwd.to_str().unwrap();
    assert!(
        text.chars()
            .all(|c| c == '/' || c.is_ascii_alphanumeric() || "._-".contains(c)),
        "{text}"
    );
    format!("--{}--", text.trim_start_matches('/').replace('/', "-"))
}

const FAKE_DSH: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$PWD/args.txt"
env | grep -E '^(DSH_|DEEPSEEK_|TMPDIR=)' | sort > "$PWD/env.txt"
cat > "$PWD/stdin.txt"
cp "$3" "$PWD/patch.txt"
ls -A "$DSH_HOME" "$DSH_AGENTS_HOME" > "$PWD/dirs.txt"
cat <<'LINES'
{"type":"session","sessionId":"session-1e0e","cwd":"/work"}
{"type":"status","phase":"turn_start","turn":1}
{"type":"status","phase":"step_start","turn":1,"step":1}
{"type":"thinking","text":""}
{"type":"text","text":"Running it."}
{"type":"tool_call","callId":"call_1","tool":"bash","input":{"command":"echo hi\necho there","description":"print"}}
{"type":"tool_result","callId":"call_1","status":"error","result":"exit 1"}
{"type":"status","phase":"step_end","turn":1,"step":1,"usage":{"inputTokens":1,"outputTokens":2,"cacheReadTokens":3,"cacheWriteTokens":4,"totalTokens":10}}
{"type":"status","phase":"step_start","turn":1,"step":2}
{"type":"text","text":"hi there"}
{"type":"status","phase":"step_end","turn":1,"step":2,"usage":{"inputTokens":5,"outputTokens":6,"cacheReadTokens":7,"cacheWriteTokens":0,"totalTokens":18}}
{"type":"status","phase":"turn_end","turn":1,"reason":{"kind":"completed"}}
{"type":"final","text":"hi there, in full"}
LINES
"#;

#[test]
fn fake_dsh_run_without_sandbox() {
    let env = Env::with("dsh", FAKE_DSH);
    let mut command = command(
        &env,
        &[
            "--sandbox",
            "off",
            "--model",
            "deepseek-v4-pro",
            "--effort",
            "low",
            "--no-subagents",
            "--prompt=-n say hi\n",
            "--",
            "--verbose",
        ],
    );
    command
        .env("DSH_HOME", env.home().join(".dsh"))
        .env("DSH_TOOLS_MODE", "all");
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = support::events(&output);
    let executable = env.bin().join("dsh").to_string_lossy().into_owned();
    let argv = events[0]["argv"].as_array().unwrap();
    assert_eq!(
        argv[..3],
        [json!(executable), json!("headless"), json!("--patch")]
    );
    assert_eq!(argv[4..], [json!("--json"), json!("--verbose"), json!("-")]);
    assert_eq!(events[0]["runtime"], "dsh");
    assert_eq!(events[0]["env"], json!([]));
    assert_eq!(read(env.work().join("stdin.txt")), "-n say hi\n");
    let tempdir = PathBuf::from(argv[3].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    assert_eq!(tempdir.parent().unwrap(), env.tmp());
    assert_eq!(
        read(env.work().join("env.txt")),
        format!(
            "DEEPSEEK_API_KEY={KEY}\nDSH_AGENTS_HOME={agents}\nDSH_HOME={home}\nDSH_PERMISSION_MODE=danger-full-access\nDSH_TELEMETRY_DISABLED=1\nTMPDIR={tmp}\n",
            agents = tempdir.join("dsh-agents").display(),
            home = tempdir.join("dsh-home").display(),
            tmp = tempdir.display(),
        )
    );
    assert_eq!(
        read(env.work().join("dirs.txt"))
            .trim_end()
            .lines()
            .filter(|line| !line.is_empty() && !line.ends_with(':'))
            .count(),
        0
    );
    let patch: Value = serde_json::from_str(&read(env.work().join("patch.txt"))).unwrap();
    let ids: Vec<&str> = patch
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "agent-instructions",
            "skill-filesystem",
            "session-log-deepseek",
            "plugin-package-inventory-deepseek",
            "session-telemetry-otel",
            "session-title-llm",
            "tool-web",
            "tool-subagent",
            "tool-subagent-fork",
            "tool-workflow",
            "tool-subagent-control",
            "tool-subagent-list-agents",
            "agent-default-model",
            "llm-deepseek",
        ]
    );
    assert_eq!(
        patch[12]["config"],
        json!({"provider": "deepseek-official", "model": "deepseek-v4-pro"})
    );
    assert_eq!(patch[13]["config"], json!({"reasoningEffort": "low"}));
    assert_eq!(
        events[1..],
        [
            json!({"schema": 1, "type": "prompt", "text": "-n say hi\n", "images": []}),
            json!({"schema": 1, "type": "text", "parent": null, "text": "Running it."}),
            json!({"schema": 1, "type": "tool_start", "id": "call_1", "parent": null, "name": "bash", "summary": "bash: echo hi"}),
            json!({"schema": 1, "type": "tool", "id": "call_1", "parent": null, "name": "bash", "summary": "bash: echo hi", "denied": false, "error": true}),
            json!({"schema": 1, "type": "usage", "parent": null, "model": "deepseek-v4-pro", "input_tokens": 1, "output_tokens": 2,
                "cache_read_tokens": 3, "cache_write_tokens": 4, "cost_usd": null, "context_tokens": 8}),
            json!({"schema": 1, "type": "text", "parent": null, "text": "hi there"}),
            json!({"schema": 1, "type": "usage", "parent": null, "model": "deepseek-v4-pro", "input_tokens": 5, "output_tokens": 6,
                "cache_read_tokens": 7, "cache_write_tokens": 0, "cost_usd": null, "context_tokens": 12}),
            json!({"schema": 1, "type": "end", "status": "finished", "exit_code": 0, "detail": "",
                "usage": {"input_tokens": 6, "output_tokens": 8, "cache_read_tokens": 10, "cache_write_tokens": 4, "cost_usd": null,
                    "by_model": {"deepseek-v4-pro": {"input_tokens": 6, "output_tokens": 8, "cache_read_tokens": 10, "cache_write_tokens": 4, "cost_usd": null}}},
                "result": "hi there, in full", "session_id": null}),
        ]
    );
    assert!(env.leftovers().is_empty());
}

#[test]
fn dry_run_prints_the_command_and_the_removed_dsh_variables() {
    let env = Env::with("dsh", FAKE_DSH);
    let mut command = command(&env, &["--sandbox", "off", "--dry-run", "--prompt", "hi"]);
    command.env("DSH_HOME", "/x");
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let executable = env.bin().join("dsh");
    assert_eq!(
        stdout.lines().next().unwrap(),
        format!(
            "command: {} headless --patch '<tempdir>/dsh-patch.yml' --json -",
            executable.display()
        )
    );
    assert!(stdout.contains("\nremoved: DSH_HOME\n"), "{stdout}");
    assert!(env.leftovers().is_empty());
}

#[test]
fn missing_key_and_images_are_rejected_before_dsh_starts() {
    let env = Env::with("dsh", "#!/bin/sh\ntouch \"$PWD/started\"\n");
    let image = env.work().join("a.png");
    std::fs::write(&image, b"\x89PNG").unwrap();
    let cases: [(&[&str], bool, &str); 2] = [
        (
            &["--prompt", "hi"],
            false,
            "DEEPSEEK_API_KEY is not set in the session environment. Export it, or pass it with --env DEEPSEEK_API_KEY=<key>",
        ),
        (
            &["--image", image.to_str().unwrap(), "--prompt", "hi"],
            true,
            "--image is not supported by dsh: its headless mode takes text only",
        ),
    ];
    for (args, key, detail) in cases {
        let mut command = command(&env, &[&["--sandbox", "off"], args].concat());
        if !key {
            command.env_remove("DEEPSEEK_API_KEY");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let end = support::events(&output).pop().unwrap();
        assert_eq!(end["status"], "rejected");
        assert_eq!(end["detail"], detail);
    }
    assert!(!env.work().join("started").exists());
    assert!(env.leftovers().is_empty());
}

const FAILING_DSH: &str = r#"#!/bin/sh
cat > /dev/null
echo '{"type":"session","sessionId":"session-1","cwd":"/w"}'
echo '{"type":"status","phase":"turn_end","turn":1,"reason":{"kind":"error","error":{"message":"missing key","code":"MISSING_CREDENTIAL"}}}'
echo '{"type":"final","text":""}'
echo 'dsh: MISSING_CREDENTIAL: missing key' >&2
exit 1
"#;

#[test]
fn turn_end_error_is_the_failure_detail() {
    let env = Env::with("dsh", FAILING_DSH);
    let output = run(&env, &["--sandbox", "off", "--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["status"], "failed");
    assert_eq!(end["exit_code"], 1);
    assert_eq!(end["detail"], "MISSING_CREDENTIAL: missing key");
    assert_eq!(end["result"], Value::Null);
}

const LIMITED_DSH: &str = r#"#!/bin/sh
trap 'echo "{\"type\":\"final\",\"text\":\"partial\"}"; exit 0' TERM
cat > /dev/null
echo '{"type":"session","sessionId":"session-1","cwd":"/w"}'
echo '{"type":"status","phase":"turn_start","turn":1}'
echo '{"type":"status","phase":"step_start","turn":1,"step":1}'
echo '{"type":"status","phase":"step_end","turn":1,"step":1,"usage":{"inputTokens":1,"outputTokens":2,"cacheReadTokens":3,"cacheWriteTokens":4,"totalTokens":10}}'
echo '{"type":"status","phase":"step_start","turn":1,"step":2}'
while :; do sleep 1; done
"#;

#[test]
fn limits_stop_dsh_and_the_result_is_limit() {
    let env = Env::with("dsh", LIMITED_DSH);
    let cases: [(&[&str], &str); 2] = [
        (
            &["--max-turns", "1"],
            "--max-turns 1 reached: the main agent started turn 2",
        ),
        (&["--max-tokens", "9"], "--max-tokens 9 exceeded: 10 tokens"),
    ];
    for (limit, detail) in cases {
        let output = run(
            &env,
            &[&["--sandbox", "off", "--prompt", "hi"], limit].concat(),
        );
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        let end = support::events(&output).pop().unwrap();
        assert_eq!(end["status"], "limit");
        assert_eq!(end["exit_code"], 0);
        assert_eq!(end["detail"], detail);
        assert_eq!(end["result"], "partial");
    }
    assert!(env.leftovers().is_empty());
}

const OVER_LIMIT_DSH: &str = r#"#!/bin/sh
trap '' TERM
cat > /dev/null
echo '{"type":"session","sessionId":"session-1","cwd":"/w"}'
echo '{"type":"status","phase":"step_start","turn":1,"step":1}'
echo '{"type":"status","phase":"step_end","turn":1,"step":1,"usage":{"inputTokens":1,"outputTokens":2,"cacheReadTokens":3,"cacheWriteTokens":4,"totalTokens":10}}'
echo '{"type":"status","phase":"turn_end","turn":1,"reason":{"kind":"completed"}}'
echo '{"type":"final","text":"done"}'
"#;

#[test]
fn a_run_that_completed_its_turn_is_finished_even_over_the_limit() {
    let env = Env::with("dsh", OVER_LIMIT_DSH);
    let output = run(
        &env,
        &["--sandbox", "off", "--max-tokens", "9", "--prompt", "hi"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["status"], "finished");
    assert_eq!(end["result"], "done");
}

const SESSION_DSH: &str = r#"#!/bin/sh
cat > /dev/null
if [ "$5" = --session-id ]; then
  name=$6
  found=$(ls -d "$DSH_HOME"/sessions/*/"$name")
  printf '%s\n' "$found" > "$PWD/resumed.txt"
  cat "$found/session.v4.jsonl.zstd" >> "$PWD/resumed.txt"
  echo second >> "$found/session.v4.jsonl.zstd"
else
  name=session-bd2ebf55-0917-4915-8793-20a5fc640cb9
  dir="$DSH_HOME/sessions/--elsewhere--/$name"
  mkdir -p "$dir"
  echo first > "$dir/session.v4.jsonl.zstd"
  : > "$dir/session.lock"
fi
echo "{\"type\":\"session\",\"sessionId\":\"$name\",\"cwd\":\"$PWD\"}"
echo '{"type":"status","phase":"turn_end","turn":1,"reason":{"kind":"completed"}}'
echo '{"type":"final","text":"ok"}'
"#;

#[test]
fn session_dir_saves_the_session_and_resume_restores_it_under_the_cwd() {
    let env = Env::with("dsh", SESSION_DSH);
    let store = env.root().join("sessions");
    let store_arg = store.to_str().unwrap();
    let output = run(
        &env,
        &[
            "--sandbox",
            "off",
            "--session-dir",
            store_arg,
            "--prompt",
            "hi",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["session_id"], SESSION_ID);
    let saved = store.join("dsh").join(format!("session-{SESSION_ID}"));
    assert_eq!(read(saved.join("session.v4.jsonl.zstd")), "first\n");
    assert!(env.leftovers().is_empty());

    let output = run(
        &env,
        &[
            "--sandbox",
            "off",
            "--session-dir",
            store_arg,
            "--resume",
            SESSION_ID,
            "--prompt",
            "again",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = support::events(&output);
    let argv = events[0]["argv"].as_array().unwrap();
    assert_eq!(
        argv[4..],
        [
            json!("--json"),
            json!("--session-id"),
            json!(format!("session-{SESSION_ID}")),
            json!("-")
        ]
    );
    assert_eq!(events.last().unwrap()["session_id"], SESSION_ID);
    let resumed = read(env.work().join("resumed.txt"));
    let mut lines = resumed.lines();
    let found = PathBuf::from(lines.next().unwrap());
    let work = std::fs::canonicalize(env.work()).unwrap();
    assert_eq!(
        found
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        project_key(&work)
    );
    assert_eq!(lines.collect::<Vec<_>>(), ["first"]);
    assert_eq!(read(saved.join("session.v4.jsonl.zstd")), "first\nsecond\n");
    assert!(env.leftovers().is_empty());

    let output = run(
        &env,
        &[
            "--sandbox",
            "off",
            "--session-dir",
            store_arg,
            "--resume",
            "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b",
            "--prompt",
            "again",
        ],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let end = support::events(&output).pop().unwrap();
    assert_eq!(
        end["detail"],
        format!(
            "no saved dsh session 0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b in {}",
            store.join("dsh").display()
        )
    );
}

const PROXY_DSH: &str = r#"#!/bin/sh
cat > /dev/null
env | grep -E '^(https?_proxy|HTTPS?_PROXY|NO_PROXY|no_proxy)=' | sort > "$PWD/proxy.txt"
echo '{"type":"status","phase":"turn_end","turn":1,"reason":{"kind":"completed"}}'
"#;

#[test]
fn sandboxed_run_sets_the_proxy_variables() {
    if !support::sandbox_available() {
        eprintln!("skipped: the sandbox is not available");
        return;
    }
    let env = Env::with("dsh", PROXY_DSH);
    let output = run(&env, &["--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = support::events(&output);
    assert_ne!(events[0]["sandbox"], "none");
    let proxy = read(env.work().join("proxy.txt"));
    assert!(
        proxy
            .lines()
            .any(|line| line.starts_with("HTTPS_PROXY=http://127.0.0.1:")),
        "{proxy}"
    );
    assert!(proxy.contains("NO_PROXY=\n"), "{proxy}");
    assert!(env.leftovers().is_empty());
}

#[test]
fn an_unreadable_base_url_is_rejected_before_dsh_starts() {
    if !support::sandbox_available() {
        eprintln!("skipped: the sandbox is not available");
        return;
    }
    let env = Env::with("dsh", "#!/bin/sh\ntouch \"$PWD/started\"\n");
    for url in [
        "http://[::1]:8080",
        "https://host.example:abc",
        "api.deepseek.com",
    ] {
        let output = command(&env, &["--network", "none", "--prompt", "hi"])
            .env("DEEPSEEK_BASE_URL", url)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{url}: {output:?}");
        let end = support::events(&output).pop().unwrap();
        assert_eq!(end["status"], "rejected");
        assert_eq!(
            end["detail"],
            "cannot tell which host dsh's model service uses from DEEPSEEK_BASE_URL. Use --network custom --allow-host <host of the model service>"
        );
    }
    assert!(!env.work().join("started").exists());
    assert!(env.leftovers().is_empty());
}
