#[allow(dead_code)]
mod support;

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use agentrun::codex::Codex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use support::env::Env;

fn replay(name: &str, prompt: &str) {
    support::assert_replay(&mut Codex::new(), "codex", name, prompt);
}

#[test]
fn replay_basic_task() {
    replay(
        "basic-task",
        "Create a file named probe.txt in the current working directory containing exactly the text: hello probe. Then read it back and reply with its contents.",
    );
}

#[test]
fn replay_outside_write() {
    replay(
        "outside-write",
        "Write a file at the absolute path /Users/user/agentrun-probe-outside-2e2096a9 containing the text: outside. Use whatever tool is available: a file writing tool or a shell command. Reply with one line saying whether the write succeeded or was blocked, and quote any error message.",
    );
}

#[test]
fn replay_network() {
    replay(
        "network",
        "Run these two shell commands and reply with one line each giving only the HTTP status code printed, or the error message if it fails: curl -sS -m 20 -o /dev/null -w '%{http_code}' https://api.github.com/ ; curl -sS -m 20 -o /dev/null -w '%{http_code}' https://example.com/",
    );
}

const SUBAGENT_PROMPT: &str = "Start two subagents in parallel. The first subagent must create a file sub1.txt in the current working directory containing the text: one. The second subagent must create sub2.txt containing the text: two. Wait for both to finish, then reply with the word done. If you have no way to start subagents, say so and do not create the files yourself.";

#[test]
fn replay_subagents() {
    replay("subagents", SUBAGENT_PROMPT);
}

#[test]
fn replay_no_subagents() {
    replay("no-subagents", SUBAGENT_PROMPT);
}

#[test]
fn replay_model_error() {
    replay("model-error", "Reply with the single word OK.");
}

#[test]
fn replay_cred_error() {
    replay("cred-error", "Reply with the single word OK.");
}

#[test]
fn replay_arg_error() {
    replay("arg-error", "Reply with the single word OK.");
}

#[test]
fn replay_sigint() {
    replay(
        "sigint",
        "Use the file writing tool to create a file named counting.txt in the current working directory whose content is the integers from 1 to 1500, one per line, written out in full in a single write call. Do not use a shell command to generate it. Then reply with the word done.",
    );
}

fn with_login(script: &str) -> Env {
    let env = Env::with("codex", script);
    write_login(&env);
    env
}

fn login(env: &Env) -> PathBuf {
    env.home().join(".codex/auth.json")
}

fn write_login(env: &Env) -> PathBuf {
    let path = login(env);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{\"tokens\":{}}").unwrap();
    path
}

fn command(env: &Env, args: &[&str]) -> Command {
    let work = env.work();
    env.command(&[&["codex", "--cwd", work.to_str().unwrap()], args].concat())
}

fn run(env: &Env, args: &[&str]) -> Output {
    command(env, args).output().unwrap()
}

const FAKE_CODEX: &str = r#"#!/bin/sh
cat <<'LINES'
{"type":"thread.started","thread_id":"0199-fake"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"working"}}
{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"ls\npwd","aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls\npwd","aggregated_output":"a.txt","exit_code":0,"status":"completed"}}
{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"done"}}
{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":40,"cache_write_input_tokens":2,"output_tokens":7,"reasoning_output_tokens":1}}
LINES
"#;

#[test]
fn fake_codex_run_without_sandbox() {
    let env = with_login(FAKE_CODEX);
    let output = run(
        &env,
        &[
            "--sandbox",
            "off",
            "--model",
            "gpt-5.3-codex",
            "--effort",
            "low",
            "--no-subagents",
            "--prompt",
            "say hi",
            "--",
            "-c",
            "x=1",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let events = support::events(&output);
    let start = &events[0];
    let executable = env.bin().join("codex").to_string_lossy().into_owned();
    let mut argv = vec![
        executable,
        "exec".to_string(),
        "--json".to_string(),
        "--skip-git-repo-check".to_string(),
        "--ignore-user-config".to_string(),
        "--ignore-rules".to_string(),
        "-C".to_string(),
        env.work().to_string_lossy().into_owned(),
        "-c".to_string(),
        "default_permissions=\":danger-full-access\"".to_string(),
        "-c".to_string(),
        "approval_policy=\"never\"".to_string(),
        "-c".to_string(),
        "web_search=\"disabled\"".to_string(),
        "-c".to_string(),
        "project_doc_max_bytes=0".to_string(),
        "-c".to_string(),
        "skills.include_instructions=false".to_string(),
        "-c".to_string(),
        "skills.bundled.enabled=false".to_string(),
        "-c".to_string(),
        "allow_login_shell=false".to_string(),
    ];
    for feature in [
        "apps",
        "plugins",
        "remote_plugin",
        "plugin_sharing",
        "tool_suggest",
        "hooks",
        "skill_mcp_dependency_install",
        "image_generation",
        "goals",
        "memories",
        "shell_snapshot",
        "computer_use",
        "browser_use",
        "browser_use_external",
        "in_app_browser",
        "daemon_auto_start",
    ] {
        argv.push("--disable".to_string());
        argv.push(feature.to_string());
    }
    argv.extend(
        [
            "-c",
            "agents.enabled=false",
            "--disable",
            "multi_agent",
            "--disable",
            "multi_agent_v2",
            "-m",
            "gpt-5.3-codex",
            "-c",
            "model_reasoning_effort=\"low\"",
            "-c",
            "x=1",
            "-",
        ]
        .map(str::to_string),
    );
    assert_eq!(start["argv"], json!(argv));
    assert_eq!(start["sandbox"], "none");
    assert_eq!(start["model"], "gpt-5.3-codex");
    assert_eq!(
        start["network"],
        json!({"mode": "none", "allow": [], "enforced": false})
    );
    assert_eq!(start["env"], json!([]));
    assert_eq!(
        events[1..],
        [
            json!({"schema": 1, "type": "prompt", "text": "say hi"}),
            json!({"schema": 1, "type": "text", "parent": null, "text": "working"}),
            json!({"schema": 1, "type": "tool", "id": "item_1", "parent": null, "name": "command_execution", "summary": "shell: ls", "denied": false}),
            json!({"schema": 1, "type": "text", "parent": null, "text": "done"}),
            json!({"schema": 1, "type": "usage", "parent": null, "model": "gpt-5.3-codex", "input_tokens": 60, "output_tokens": 7,
                "cache_read_tokens": 40, "cache_write_tokens": 2, "context_tokens": 102}),
            json!({"schema": 1, "type": "end", "status": "finished", "exit_code": 0, "detail": "",
                "usage": {"input_tokens": 60, "output_tokens": 7, "cache_read_tokens": 40, "cache_write_tokens": 2,
                    "by_model": {"gpt-5.3-codex": {"input_tokens": 60, "output_tokens": 7, "cache_read_tokens": 40, "cache_write_tokens": 2}}},
                "result": "done"}),
        ]
    );
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
}

const REPORTING_CODEX: &str = r#"#!/bin/sh
cat > "$PWD/prompt.txt"
{
  echo "home=$CODEX_HOME"
  echo "tmpdir=$TMPDIR"
  echo "auth=$(readlink "$CODEX_HOME/auth.json")"
  echo "entries=$(ls -A "$CODEX_HOME" | sort | tr '\n' ' ')"
  echo "mode=$(stat -c %a "$CODEX_HOME" 2>/dev/null || stat -f %Lp "$CODEX_HOME")"
  echo "proxy=${http_proxy-unset}"
  echo "auth_var=${AGENTRUN_CODEX_AUTH-unset}"
} > "$PWD/state.txt"
echo '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"ok"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":1}}'
"#;

fn field(report: &str, name: &str) -> String {
    report
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("{name} in {report}"))
        .to_string()
}

#[test]
fn private_home_links_the_login_file_and_is_removed_afterwards() {
    let env = Env::with("codex", REPORTING_CODEX);
    let login = write_login(&env);
    let output = run(
        &env,
        &["--sandbox", "off", "--prompt", "two\nlines \"quoted\""],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report = std::fs::read_to_string(env.work().join("state.txt")).unwrap();
    let home = PathBuf::from(field(&report, "home"));
    let tempdir = PathBuf::from(field(&report, "tmpdir"));
    assert_eq!(home.parent().unwrap(), env.tmp());
    assert_eq!(
        home.file_name().unwrap().to_string_lossy(),
        format!("{}-codex", tempdir.file_name().unwrap().to_string_lossy())
    );
    assert_eq!(PathBuf::from(field(&report, "auth")), login);
    assert_eq!(field(&report, "entries"), "auth.json ");
    assert_eq!(field(&report, "mode"), "700");
    assert_eq!(
        std::fs::read_to_string(env.work().join("prompt.txt")).unwrap(),
        "two\nlines \"quoted\""
    );
    assert_eq!(field(&report, "proxy"), "unset");
    assert!(!home.exists());
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
    assert_eq!(std::fs::read_to_string(&login).unwrap(), "{\"tokens\":{}}");
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["status"], "finished");
    assert_eq!(end["result"], "ok");
}

#[test]
fn codex_auth_variable_is_written_and_linked_into_the_private_home() {
    const VALUE: &str = r#"{"tokens":{"id_token":"codex-credential-5d1e8b"}}"#;
    let env = Env::with("codex", REPORTING_CODEX);
    let args = ["--sandbox", "off", "--debug", "--prompt", "hi"];
    let output = command(&env, &args)
        .env("AGENTRUN_CODEX_AUTH", VALUE)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["status"], "finished");
    assert_eq!(end["result"], "ok");
    let login = login(&env);
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(std::fs::read_to_string(&login).unwrap(), VALUE);
    assert_eq!(mode(&login), 0o600);
    assert_eq!(mode(login.parent().unwrap()), 0o700);
    let digest: String = Sha256::digest(VALUE.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        std::fs::read_to_string(login.with_file_name("auth.json.agentrun-sha256")).unwrap(),
        format!("{digest}\n")
    );
    let report = std::fs::read_to_string(env.work().join("state.txt")).unwrap();
    assert_eq!(PathBuf::from(field(&report, "auth")), login);
    assert_eq!(field(&report, "entries"), "auth.json ");
    assert_eq!(field(&report, "mode"), "700");
    assert_eq!(field(&report, "auth_var"), "unset");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!(
            "[debug] credentials: AGENTRUN_CODEX_AUTH -> {} (written)\n",
            login.display()
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains("[debug] agentrun variables: AGENTRUN_CODEX_AUTH\n"),
        "{stderr}"
    );
    for text in [&stdout, &stderr] {
        assert!(!text.contains("codex-credential-5d1e8b"), "{text}");
    }

    let output = command(&env, &args)
        .env("AGENTRUN_CODEX_AUTH", VALUE)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!(
            "[debug] credentials: AGENTRUN_CODEX_AUTH -> {} (unchanged)\n",
            login.display()
        )),
        "{stderr}"
    );
    assert_eq!(std::fs::read_to_string(&login).unwrap(), VALUE);
}

#[test]
fn login_file_follows_codex_home_from_the_session_environment() {
    let env = Env::with("codex", REPORTING_CODEX);
    let state = env.root().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("auth.json"), "{}").unwrap();
    let variable = format!("CODEX_HOME={}", state.display());
    let output = run(
        &env,
        &["--sandbox", "off", "--env", &variable, "--prompt", "hi"],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report = std::fs::read_to_string(env.work().join("state.txt")).unwrap();
    assert_eq!(
        PathBuf::from(field(&report, "auth")),
        state.join("auth.json")
    );
    assert_ne!(PathBuf::from(field(&report, "home")), state);
    let start = &support::events(&output)[0];
    assert_eq!(start["env"], json!(["CODEX_HOME"]));
}

#[test]
fn missing_login_file_is_rejected_with_and_without_dry_run() {
    let env = Env::with("codex", FAKE_CODEX);
    let detail = format!(
        "codex login file {} not found. Run codex login, or pass its content in AGENTRUN_CODEX_AUTH",
        login(&env).display()
    );
    for extra in [&[][..], &["--dry-run"][..]] {
        let mut args = vec!["--sandbox", "off", "--prompt", "hi"];
        args.extend(extra);
        let output = run(&env, &args);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let end = support::events(&output).pop().unwrap();
        assert_eq!(end["status"], "rejected");
        assert_eq!(end["detail"], detail);
        assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
    }
    let output = run(
        &env,
        &[
            "--sandbox",
            "off",
            "--env",
            "CODEX_HOME=/nonexistent",
            "--prompt",
            "hi",
        ],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(
        support::events(&output).pop().unwrap()["detail"],
        "codex login file /nonexistent/auth.json not found. Run codex login, or pass its content in AGENTRUN_CODEX_AUTH"
    );
}

#[test]
fn dry_run_accepts_login_content_without_the_login_file() {
    let env = Env::with("codex", FAKE_CODEX);
    let output = command(&env, &["--sandbox", "off", "--dry-run", "--prompt", "hi"])
        .env("AGENTRUN_CODEX_AUTH", "{\"tokens\":{}}")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(!login(&env).exists());
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
}

#[test]
fn dry_run_shows_placeholders_and_creates_nothing() {
    let env = with_login(FAKE_CODEX);
    let output = run(&env, &["--sandbox", "off", "--dry-run", "--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines[0].ends_with(" --disable daemon_auto_start -"),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains("-c 'default_permissions=\":danger-full-access\"'"),
        "{}",
        lines[0]
    );
    assert!(!stdout.contains("<tempdir>"), "{stdout}");
    assert!(!stdout.contains("CODEX_HOME"), "{stdout}");
    assert_eq!(lines[2], "set: (none)");
    assert!(env.leftovers().is_empty());
}

#[test]
fn debug_keeps_the_private_home_and_prints_its_path() {
    let env = with_login(FAKE_CODEX);
    let output = run(&env, &["--sandbox", "off", "--debug", "--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let tempdir = stderr
        .lines()
        .find_map(|line| line.strip_prefix("[debug] tempdir: "))
        .unwrap_or_else(|| panic!("{stderr}"));
    let home = stderr
        .lines()
        .find_map(|line| line.strip_prefix("[debug] codex home: "))
        .unwrap_or_else(|| panic!("{stderr}"));
    assert_eq!(home, format!("{tempdir}-codex"));
    assert!(PathBuf::from(home).join("auth.json").exists());
    assert_eq!(env.leftovers().len(), 2);
    assert!(!stderr.contains("[debug] set: CODEX_HOME"), "{stderr}");
}

#[test]
fn failed_run_removes_the_private_home() {
    let env = with_login(
        "#!/bin/sh\necho '{\"type\":\"turn.failed\",\"error\":{\"message\":\"boom\"}}'\nexit 1\n",
    );
    let output = run(&env, &["--sandbox", "off", "--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["status"], "failed");
    assert_eq!(end["detail"], "boom");
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
}

#[test]
fn exit_without_turn_completed_fails() {
    let env = with_login("#!/bin/sh\nexit 0\n");
    let output = run(&env, &["--sandbox", "off", "--prompt", "hi"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let end = support::events(&output).pop().unwrap();
    assert_eq!(end["status"], "failed");
    assert_eq!(end["detail"], "codex produced no turn.completed");
}

#[test]
fn interrupted_run_removes_the_private_home() {
    let env = with_login(concat!(
        "#!/bin/sh\n",
        "echo '{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"ready\"}}'\n",
        "sleep 30\n",
    ));
    let mut child = command(&env, &["--sandbox", "off", "--prompt", "hi"])
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "the fake codex did not report ready"
        );
        let line = lines.next().unwrap().unwrap();
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["type"] == "text" {
            break;
        }
    }
    assert_eq!(env.leftovers().len(), 2, "{:?}", env.leftovers());
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130));
    let end: Value = serde_json::from_str(&lines.last().unwrap().unwrap()).unwrap();
    assert_eq!(end["status"], "interrupted");
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
}

const CURL_CODEX: &str = r#"#!/bin/sh
fetch() { curl -sS -m 5 -o /dev/null -w '%{http_code}' "$1" 2>/dev/null || echo failed; }
printf '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"listed=%s service=%s denied=%s proxy=%s all=%s noproxy=%s/%s"}}\n' \
  "$(fetch http://listed.test/)" "$(fetch https://api.openai.com/)" "$(fetch http://denied.test/)" \
  "$http_proxy" "$ALL_PROXY" "${NO_PROXY-unset}" "${no_proxy-unset}"
echo '{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":1}}'
"#;

#[test]
fn custom_network_runs_the_filter_proxy_with_the_service_hosts() {
    let upstream = support::WebServer::start();
    let env = with_login(CURL_CODEX);
    let upstream_address = format!("http://127.0.0.1:{}", upstream.port);
    let output = command(
        &env,
        &[
            "--network",
            "custom",
            "--allow-host",
            "listed.test",
            "--prompt",
            "fetch",
        ],
    )
    .env("http_proxy", &upstream_address)
    .env("https_proxy", &upstream_address)
    .output()
    .unwrap();
    let events = support::events(&output);
    if output.status.code() == Some(2)
        && events[0]["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with("sandbox is not available"))
    {
        eprintln!("skipped: {}", events[0]["detail"]);
        return;
    }
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(events[0]["sandbox"], "codex");
    assert_eq!(
        events[0]["network"],
        json!({"mode": "custom", "allow": ["listed.test"], "enforced": true})
    );
    let argv = events[0]["argv"].as_array().unwrap();
    assert!(argv.iter().any(|arg| arg == "network_proxy"), "{argv:?}");
    assert!(
        argv.iter().any(|arg| arg
            .as_str()
            .unwrap()
            .ends_with("domains={\"listed.test\"=\"allow\"}}")),
        "{argv:?}"
    );
    let text = support::text_of(&events, 0);
    let proxy = text
        .split(' ')
        .find_map(|part| part.strip_prefix("proxy="))
        .unwrap();
    assert!(proxy.starts_with("http://127.0.0.1:"), "{text}");
    assert_ne!(proxy, upstream_address);
    assert!(text.starts_with("listed=200 service="), "{text}");
    assert!(
        text.ends_with(&format!(" denied=403 proxy={proxy} all={proxy} noproxy=/")),
        "{text}"
    );
    assert_eq!(
        support::network_events(&events),
        [
            json!({"schema": 1, "type": "network", "host": "listed.test", "port": 80, "allowed": true, "reason": null}),
            json!({"schema": 1, "type": "network", "host": "api.openai.com", "port": 443, "allowed": true, "reason": null}),
            json!({"schema": 1, "type": "network", "host": "denied.test", "port": 80, "allowed": false, "reason": "not_allowed"}),
        ]
    );
    assert_eq!(upstream.served(), 2);
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
}
