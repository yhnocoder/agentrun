#[path = "support/harness.rs"]
mod harness;
#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::env::{Agentrun, Env, Finished, wait_until};
use support::fake::SIGNAL_IN_LAUNCH;
use support::process::{self, READY_FILE, describe, read_pid, spawn_role, wait_until_gone};

const READY: &str = r#"{"record":"text","parent":null,"text":"ready"}"#;
const GOT_SIGNAL: &str = r#"{\"record\":\"text\",\"parent\":null,\"text\":\"got signal\"}"#;

fn main() {
    process::take_over_if_spawned();
    harness::run_tests(tests());
}

fn tests() -> Vec<(&'static str, fn())> {
    harness::test_list![
        background_process_is_killed_after_exit,
        detached_process_is_killed_after_exit,
        detached_process_is_killed_after_interrupt,
        detached_process_is_killed_after_timeout,
        detached_process_in_sandbox_is_killed_after_exit,
        orphan_in_a_new_process_group_is_killed_after_exit,
        sigint_is_forwarded_to_the_runtime,
        sigterm_is_forwarded_to_the_runtime,
        ignored_signal_leads_to_sigkill_after_grace,
        second_signal_kills_immediately,
        timeout_terminates_the_runtime,
        timeout_with_ignored_sigterm_kills_after_grace,
        signal_during_timeout_grace_kills_immediately,
        signal_before_launch_ends_without_start,
        signal_during_a_failing_launch_still_rejects,
        adapter_can_terminate_the_process_group,
        #[cfg(target_os = "linux")]
        signal_during_sandbox_check_kills_the_check,
        first_signal_reaches_the_wrapped_runtime_directly,
    ]
}

fn with_claude(body: &str) -> Env {
    let env = Env::new();
    env.install("claude", &format!("#!/bin/sh\n{body}"));
    env
}

fn start(env: &Env, extra: &[&str], prompt: bool, stdin: Stdio) -> Agentrun {
    let work = env.work();
    let mut args = vec!["claude-code"];
    if !extra.contains(&"--sandbox") {
        args.extend(["--sandbox", "off"]);
    }
    args.extend(["--cwd", work.to_str().unwrap()]);
    if prompt {
        args.extend(["--prompt", "hi"]);
    }
    args.extend(extra);
    let mut command = env.fake_command(&args);
    command
        .env("PIDFILE", env.root().join("pid"))
        .env(READY_FILE, env.root().join("ready"))
        .stdin(stdin);
    Agentrun::spawn(command)
}

fn finish(agentrun: Agentrun, env: &Env) -> Finished {
    let finished = agentrun.wait();
    finished.end();
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
    finished
}

fn assert_between(elapsed: Duration, low: f64, high: f64, events: &[Value]) {
    let seconds = elapsed.as_secs_f64();
    assert!(
        seconds >= low && seconds <= high,
        "took {seconds:.2}s, expected between {low}s and {high}s; events {events:?}"
    );
}

fn spawn_detached_sleeper(pidfile: &str) -> String {
    spawn_role("detached", pidfile)
}

fn assert_sleeper_gone(pidfile: &Path) {
    let pid = read_pid(pidfile);
    assert!(
        wait_until_gone(pid),
        "detached sleeper {pid} is still running after agentrun exited: {}",
        describe(pid)
    );
}

fn detached_process_is_killed_after_exit() {
    let env = with_claude(&format!(
        "{}sleep 2\nexit 0\n",
        spawn_detached_sleeper("$PIDFILE")
    ));
    let outcome = finish(start(&env, &[], true, Stdio::null()), &env);
    assert_eq!(outcome.code, 0);
    assert_eq!(outcome.end()["status"], "finished");
    assert_between(outcome.elapsed, 1.5, 4.0, &outcome.events);
    assert_sleeper_gone(&env.root().join("pid"));
}

fn detached_process_is_killed_after_interrupt() {
    let env = with_claude(&format!(
        "{}trap 'exit 0' INT\necho '{READY}'\nwhile :; do sleep 1; done\n",
        spawn_detached_sleeper("$PIDFILE")
    ));
    let agentrun = start(&env, &[], true, Stdio::null());
    agentrun.wait_for_text("ready");
    agentrun.signal(libc::SIGINT);
    let outcome = finish(agentrun, &env);
    assert_eq!(outcome.code, 130);
    assert_eq!(outcome.end()["status"], "interrupted");
    assert_sleeper_gone(&env.root().join("pid"));
}

fn detached_process_is_killed_after_timeout() {
    let env = with_claude(&format!(
        "{}echo '{READY}'\nsleep 30\n",
        spawn_detached_sleeper("$PIDFILE")
    ));
    let outcome = finish(start(&env, &["--timeout", "3"], true, Stdio::null()), &env);
    assert_eq!(outcome.code, 3);
    assert_eq!(outcome.end()["status"], "timeout");
    assert_sleeper_gone(&env.root().join("pid"));
}

fn orphan_in_a_new_process_group_is_killed_after_exit() {
    let env = with_claude(&format!(
        "{}sleep 2\nexit 0\n",
        spawn_role("shell", "$PIDFILE")
    ));
    let outcome = finish(start(&env, &[], true, Stdio::null()), &env);
    assert_eq!(outcome.code, 0);
    assert_eq!(outcome.end()["status"], "finished");
    assert_sleeper_gone(&env.root().join("pid"));
}

fn detached_process_in_sandbox_is_killed_after_exit() {
    if !support::sandbox_available() {
        return;
    }
    let env = with_claude(&format!(
        "{}sleep 2\nexit 0\n",
        spawn_detached_sleeper("$PWD/pid")
    ));
    let outcome = finish(start(&env, &["--sandbox", "on"], true, Stdio::null()), &env);
    assert_eq!(outcome.code, 0);
    assert_eq!(outcome.end()["status"], "finished");
    assert_eq!(outcome.events[0]["sandbox"], support::SANDBOX_KIND);
    assert_sleeper_gone(&env.work().join("pid"));
}

fn background_process_is_killed_after_exit() {
    let env = with_claude("sleep 30 &\necho $! > \"$PIDFILE\"\nexit 0\n");
    let outcome = finish(start(&env, &[], true, Stdio::null()), &env);
    assert_eq!(outcome.code, 0);
    assert_eq!(outcome.end()["status"], "finished");
    assert_between(outcome.elapsed, 0.0, 10.0, &outcome.events);
    let pid = read_pid(&env.root().join("pid"));
    assert!(
        wait_until_gone(pid),
        "background sleep {pid} is still running"
    );
}

fn forwarded_signal(name: &str, signal: i32, code: i32) {
    let env = with_claude(&format!(
        "trap 'printf \"%s\\n\" \"{GOT_SIGNAL}\"; exit 0' {name}\necho '{READY}'\nwhile :; do sleep 1; done\n"
    ));
    let agentrun = start(&env, &[], true, Stdio::null());
    agentrun.wait_for_text("ready");
    let sent = Instant::now();
    agentrun.signal(signal);
    let outcome = finish(agentrun, &env);
    assert_between(sent.elapsed(), 0.0, 2.0, &outcome.events);
    assert_eq!(outcome.code, code);
    assert_eq!(outcome.end()["status"], "interrupted");
    assert_eq!(outcome.end()["exit_code"], 0);
    assert_eq!(outcome.end()["detail"], "");
    assert_eq!(outcome.texts(), vec!["ready", "got signal"]);
}

fn sigint_is_forwarded_to_the_runtime() {
    forwarded_signal("INT", libc::SIGINT, 130);
}

fn sigterm_is_forwarded_to_the_runtime() {
    forwarded_signal("TERM", libc::SIGTERM, 143);
}

fn ignored_signal_leads_to_sigkill_after_grace() {
    let env = with_claude(&format!("trap '' TERM\necho '{READY}'\nsleep 30\n"));
    let agentrun = start(&env, &[], true, Stdio::null());
    agentrun.wait_for_text("ready");
    let sent = Instant::now();
    agentrun.signal(libc::SIGTERM);
    let outcome = finish(agentrun, &env);
    assert_between(sent.elapsed(), 4.5, 9.0, &outcome.events);
    assert_eq!(outcome.code, 143);
    assert_eq!(outcome.end()["status"], "interrupted");
    assert_eq!(outcome.end()["exit_code"], Value::Null);
}

fn second_signal_kills_immediately() {
    let env = with_claude(&format!(
        "trap ': > \"$PWD/got-int\"' INT\necho '{READY}'\nwhile :; do sleep 1 & wait $!; done\n"
    ));
    let agentrun = start(&env, &[], true, Stdio::null());
    agentrun.wait_for_text("ready");
    agentrun.signal(libc::SIGINT);
    let got_int = env.work().join("got-int");
    wait_until("the fake claude to run its INT trap", || got_int.exists());
    let sent = Instant::now();
    agentrun.signal(libc::SIGINT);
    let outcome = finish(agentrun, &env);
    assert_between(sent.elapsed(), 0.0, 1.0, &outcome.events);
    assert_eq!(outcome.code, 130);
    assert_eq!(outcome.end()["status"], "interrupted");
    assert_eq!(outcome.end()["exit_code"], Value::Null);
}

fn timeout_terminates_the_runtime() {
    let env = with_claude(&format!("echo '{READY}'\nsleep 30\n"));
    let outcome = finish(start(&env, &["--timeout", "3"], true, Stdio::null()), &env);
    assert_between(outcome.elapsed, 2.9, 5.0, &outcome.events);
    assert_eq!(outcome.code, 3);
    assert_eq!(outcome.end()["status"], "timeout");
    assert_eq!(outcome.end()["exit_code"], Value::Null);
    assert_eq!(outcome.end()["detail"], "");
    assert_eq!(outcome.end()["result"], "ready");
}

fn timeout_with_ignored_sigterm_kills_after_grace() {
    let env = with_claude(&format!("trap '' TERM\necho '{READY}'\nsleep 30\n"));
    let outcome = finish(start(&env, &["--timeout", "3"], true, Stdio::null()), &env);
    assert_between(outcome.elapsed, 7.5, 11.0, &outcome.events);
    assert_eq!(outcome.code, 3);
    assert_eq!(outcome.end()["status"], "timeout");
}

fn signal_during_timeout_grace_kills_immediately() {
    let env = with_claude(&format!(
        "trap ': > \"$PWD/got-term\"' TERM\necho '{READY}'\nwhile :; do sleep 1 & wait $!; done\n"
    ));
    let agentrun = start(&env, &["--timeout", "3"], true, Stdio::null());
    agentrun.wait_for_text("ready");
    let got_term = env.work().join("got-term");
    wait_until("the timeout to send SIGTERM to the fake claude", || {
        got_term.exists()
    });
    let sent = Instant::now();
    agentrun.signal(libc::SIGINT);
    let outcome = finish(agentrun, &env);
    assert_between(sent.elapsed(), 0.0, 1.0, &outcome.events);
    assert_eq!(outcome.code, 130);
    assert_eq!(outcome.end()["status"], "interrupted");
}

fn signal_before_launch_ends_without_start() {
    let env = with_claude("exit 0\n");
    let mut agentrun = start(&env, &[], false, Stdio::piped());
    let mut stdin = agentrun.take_stdin();
    stdin.write_all(b"partial prompt").unwrap();
    let ready = env.root().join("ready");
    wait_until("agentrun to install its signal handlers", || ready.exists());
    agentrun.signal(libc::SIGTERM);
    let outcome = finish(agentrun, &env);
    drop(stdin);
    assert_eq!(outcome.code, 143);
    assert_eq!(outcome.events.len(), 1, "{:?}", outcome.events);
    let end = outcome.end();
    assert_eq!(end["type"], "end");
    assert_eq!(end["status"], "interrupted");
    assert_eq!(end["exit_code"], Value::Null);
    assert_eq!(end["detail"], "");
    assert_eq!(end["result"], Value::Null);
    assert_eq!(
        end["usage"],
        serde_json::json!({"input_tokens": null, "output_tokens": null, "cache_read_tokens": null, "cache_write_tokens": null, "by_model": {}})
    );
}

fn signal_during_a_failing_launch_still_rejects() {
    let env = with_claude("exit 0\n");
    let mut command = env.fake_command(&[
        "claude-code",
        "--sandbox",
        "off",
        "--cwd",
        env.work().to_str().unwrap(),
        "--prompt",
        "hi",
    ]);
    command.env(SIGNAL_IN_LAUNCH, "1");
    let outcome = finish(Agentrun::spawn(command), &env);
    assert_eq!(outcome.code, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events.len(), 1, "{:?}", outcome.events);
    let end = outcome.end();
    assert_eq!(end["status"], "rejected");
    assert_eq!(end["detail"], "launch failed after a signal");
}

#[cfg(target_os = "linux")]
fn signal_during_sandbox_check_kills_the_check() {
    let env = with_claude("exit 0\n");
    env.install(
        "bwrap",
        "#!/bin/sh\necho $$ > \"$PIDFILE\"\nexec /bin/sleep 30\n",
    );
    env.install("socat", "#!/bin/sh\nexit 0\n");
    let agentrun = start(&env, &["--sandbox", "on"], true, Stdio::null());
    let pidfile = env.root().join("pid");
    wait_until("the fake bwrap to write its pid", || {
        std::fs::read_to_string(&pidfile).is_ok_and(|text| !text.trim().is_empty())
    });
    let pid = read_pid(&pidfile);
    let sent = Instant::now();
    agentrun.signal(libc::SIGTERM);
    let outcome = finish(agentrun, &env);
    assert_between(sent.elapsed(), 0.0, 2.0, &outcome.events);
    assert_eq!(outcome.code, 143);
    assert_eq!(outcome.events.len(), 1, "{:?}", outcome.events);
    assert_eq!(outcome.end()["status"], "interrupted");
    assert!(
        wait_until_gone(pid),
        "sandbox check {pid} is still running: {}",
        describe(pid)
    );
}

fn adapter_can_terminate_the_process_group() {
    let env = with_claude(concat!(
        "echo '{\"record\":\"terminate\",\"detail\":\"model mismatch\"}'\n",
        "sleep 1\n",
        "echo '{\"record\":\"text\",\"parent\":null,\"text\":\"after\"}'\n",
    ));
    let outcome = finish(start(&env, &[], true, Stdio::null()), &env);
    assert_between(outcome.elapsed, 0.0, 2.0, &outcome.events);
    assert_eq!(outcome.code, 1);
    assert_eq!(outcome.end()["status"], "failed");
    assert_eq!(outcome.end()["detail"], "model mismatch");
    assert_eq!(outcome.end()["exit_code"], Value::Null);
    assert!(outcome.texts().is_empty(), "{:?}", outcome.texts());
}

fn first_signal_reaches_the_wrapped_runtime_directly() {
    if !support::sandbox_available() {
        return;
    }
    let env = with_claude(&format!(
        "trap 'sleep 1; : > \"$PWD/trap-ran\"; exit 0' TERM\necho '{READY}'\nwhile :; do sleep 1; done\n"
    ));
    let agentrun = start(&env, &["--sandbox", "on"], true, Stdio::null());
    agentrun.wait_for_text("ready");
    let sent = Instant::now();
    agentrun.signal(libc::SIGTERM);
    let outcome = finish(agentrun, &env);
    assert_between(sent.elapsed(), 0.9, 4.5, &outcome.events);
    assert!(
        env.work().join("trap-ran").exists(),
        "the wrapped runtime did not run its TERM trap"
    );
    assert_eq!(outcome.code, 143);
    assert_eq!(outcome.end()["status"], "interrupted");
    assert_eq!(outcome.end()["exit_code"], 0);
    assert_eq!(outcome.events[0]["sandbox"], support::SANDBOX_KIND);
}
