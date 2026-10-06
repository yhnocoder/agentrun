#[path = "support/harness.rs"]
mod harness;
#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::process::{
    self, FAKE_AGENTRUN, describe, read_pid, spawn_lock, spawn_role, wait_until_gone,
};
use tempfile::TempDir;

const READY: &str = r#"{"record":"text","parent":null,"text":"ready"}"#;
const GOT_SIGNAL: &str = r#"{\"record\":\"text\",\"parent\":null,\"text\":\"got signal\"}"#;

fn main() {
    process::take_over_if_spawned();
    let tests: Vec<(&str, fn())> = vec![
        (
            "background_process_is_killed_after_exit",
            background_process_is_killed_after_exit,
        ),
        (
            "detached_process_is_killed_after_exit",
            detached_process_is_killed_after_exit,
        ),
        (
            "detached_process_is_killed_after_interrupt",
            detached_process_is_killed_after_interrupt,
        ),
        (
            "detached_process_is_killed_after_timeout",
            detached_process_is_killed_after_timeout,
        ),
        (
            "detached_process_in_sandbox_is_killed_after_exit",
            detached_process_in_sandbox_is_killed_after_exit,
        ),
        (
            "orphan_in_a_new_process_group_is_killed_after_exit",
            orphan_in_a_new_process_group_is_killed_after_exit,
        ),
        (
            "sigint_is_forwarded_to_the_runtime",
            sigint_is_forwarded_to_the_runtime,
        ),
        (
            "sigterm_is_forwarded_to_the_runtime",
            sigterm_is_forwarded_to_the_runtime,
        ),
        (
            "ignored_signal_leads_to_sigkill_after_grace",
            ignored_signal_leads_to_sigkill_after_grace,
        ),
        (
            "second_signal_kills_immediately",
            second_signal_kills_immediately,
        ),
        (
            "timeout_terminates_the_runtime",
            timeout_terminates_the_runtime,
        ),
        (
            "timeout_with_ignored_sigterm_kills_after_grace",
            timeout_with_ignored_sigterm_kills_after_grace,
        ),
        (
            "signal_during_timeout_grace_kills_immediately",
            signal_during_timeout_grace_kills_immediately,
        ),
        (
            "signal_before_launch_ends_without_start",
            signal_before_launch_ends_without_start,
        ),
        (
            "adapter_can_terminate_the_process_group",
            adapter_can_terminate_the_process_group,
        ),
        #[cfg(target_os = "linux")]
        (
            "signal_during_sandbox_check_kills_the_check",
            signal_during_sandbox_check_kills_the_check,
        ),
        (
            "first_signal_reaches_the_wrapped_runtime_directly",
            first_signal_reaches_the_wrapped_runtime_directly,
        ),
    ];
    harness::run_tests(tests);
}

struct Env {
    root: TempDir,
}

impl Env {
    fn new(script: &str) -> Env {
        let env = Env {
            root: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir(env.bin()).unwrap();
        std::fs::create_dir(env.tmp()).unwrap();
        std::fs::create_dir(env.work()).unwrap();
        env.write_script("claude", script);
        env
    }

    fn write_script(&self, name: &str, script: &str) {
        let _guard = spawn_lock();
        let path = self.bin().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
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

    fn start(&self, extra: &[&str], prompt: bool, stdin: Stdio) -> Agentrun {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.arg("claude-code");
        if !extra.contains(&"--sandbox") {
            command.arg("--sandbox").arg("off");
        }
        command.arg("--cwd").arg(self.work());
        if prompt {
            command.arg("--prompt").arg("hi");
        }
        command
            .args(extra)
            .env_clear()
            .env(FAKE_AGENTRUN, "1")
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            .env("TMPDIR", self.tmp())
            .env("PIDFILE", self.root.path().join("pid"))
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = {
            let _guard = spawn_lock();
            command.spawn().unwrap()
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let stdout = child.stdout.take().unwrap();
        let collected = Arc::clone(&events);
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                let event: Value = serde_json::from_str(&line).unwrap();
                collected.lock().unwrap().push(event);
            }
        });
        Agentrun {
            child,
            events,
            reader: Some(reader),
            started: Instant::now(),
        }
    }

    fn leftover_tempdirs(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.tmp())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }
}

struct Agentrun {
    child: Child,
    events: Arc<Mutex<Vec<Value>>>,
    reader: Option<thread::JoinHandle<()>>,
    started: Instant,
}

struct Outcome {
    code: i32,
    events: Vec<Value>,
    elapsed: Duration,
}

impl Outcome {
    fn end(&self) -> &Value {
        self.events.last().unwrap()
    }

    fn texts(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|event| event["type"] == "text")
            .map(|event| event["text"].as_str().unwrap().to_string())
            .collect()
    }
}

impl Agentrun {
    fn wait_for_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let ready = self
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event["type"] == "text" && event["text"] == "ready");
            if ready {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "the fake runtime did not report ready; agentrun pid {} events {:?}",
            self.child.id(),
            self.events.lock().unwrap()
        );
    }

    fn signal(&self, signal: i32) {
        unsafe {
            libc::kill(self.child.id() as i32, signal);
        }
    }

    fn finish(mut self, env: &Env) -> Outcome {
        let status = self.child.wait().unwrap();
        self.reader.take().unwrap().join().unwrap();
        let elapsed = self.started.elapsed();
        let events = self.events.lock().unwrap().clone();
        assert_eq!(events.last().unwrap()["type"], "end", "{events:?}");
        assert!(
            env.leftover_tempdirs().is_empty(),
            "{:?}",
            env.leftover_tempdirs()
        );
        Outcome {
            code: status.code().unwrap(),
            events,
            elapsed,
        }
    }
}

fn assert_between(elapsed: Duration, low: f64, high: f64, events: &[Value]) {
    let seconds = elapsed.as_secs_f64();
    assert!(
        seconds >= low && seconds <= high,
        "took {seconds:.2}s, expected between {low}s and {high}s; events {events:?}"
    );
}

fn sandbox_available() -> bool {
    let _guard = spawn_lock();
    support::sandbox_available()
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
    let env = Env::new(&format!(
        "{}sleep 2\nexit 0\n",
        spawn_detached_sleeper("$PIDFILE")
    ));
    let outcome = env.start(&[], true, Stdio::null()).finish(&env);
    assert_eq!(outcome.code, 0);
    assert_eq!(outcome.end()["status"], "finished");
    assert_between(outcome.elapsed, 1.5, 4.0, &outcome.events);
    assert_sleeper_gone(&env.root.path().join("pid"));
}

fn detached_process_is_killed_after_interrupt() {
    let env = Env::new(&format!(
        "{}trap 'exit 0' INT\necho '{READY}'\nwhile :; do sleep 1; done\n",
        spawn_detached_sleeper("$PIDFILE")
    ));
    let agentrun = env.start(&[], true, Stdio::null());
    agentrun.wait_for_ready();
    agentrun.signal(libc::SIGINT);
    let outcome = agentrun.finish(&env);
    assert_eq!(outcome.code, 130);
    assert_eq!(outcome.end()["status"], "interrupted");
    assert_sleeper_gone(&env.root.path().join("pid"));
}

fn detached_process_is_killed_after_timeout() {
    let env = Env::new(&format!(
        "{}echo '{READY}'\nsleep 30\n",
        spawn_detached_sleeper("$PIDFILE")
    ));
    let outcome = env
        .start(&["--timeout", "3"], true, Stdio::null())
        .finish(&env);
    assert_eq!(outcome.code, 3);
    assert_eq!(outcome.end()["status"], "timeout");
    assert_sleeper_gone(&env.root.path().join("pid"));
}

fn orphan_in_a_new_process_group_is_killed_after_exit() {
    let env = Env::new(&format!(
        "{}sleep 2\nexit 0\n",
        spawn_role("shell", "$PIDFILE")
    ));
    let outcome = env.start(&[], true, Stdio::null()).finish(&env);
    assert_eq!(outcome.code, 0);
    assert_eq!(outcome.end()["status"], "finished");
    assert_sleeper_gone(&env.root.path().join("pid"));
}

fn detached_process_in_sandbox_is_killed_after_exit() {
    if !sandbox_available() {
        return;
    }
    let env = Env::new(&format!(
        "{}sleep 2\nexit 0\n",
        spawn_detached_sleeper("$PWD/pid")
    ));
    let outcome = env
        .start(&["--sandbox", "on"], true, Stdio::null())
        .finish(&env);
    assert_eq!(outcome.code, 0);
    assert_eq!(outcome.end()["status"], "finished");
    assert_eq!(outcome.events[0]["sandbox"], support::SANDBOX_KIND);
    assert_sleeper_gone(&env.work().join("pid"));
}

fn background_process_is_killed_after_exit() {
    let env = Env::new("sleep 30 &\necho $! > \"$PIDFILE\"\nexit 0\n");
    let outcome = env.start(&[], true, Stdio::null()).finish(&env);
    assert_eq!(outcome.code, 0);
    assert_eq!(outcome.end()["status"], "finished");
    assert_between(outcome.elapsed, 0.0, 2.0, &outcome.events);
    let pid = read_pid(&env.root.path().join("pid"));
    assert!(
        wait_until_gone(pid),
        "background sleep {pid} is still running"
    );
}

fn forwarded_signal(name: &str, signal: i32, code: i32) {
    let env = Env::new(&format!(
        "trap 'printf \"%s\\n\" \"{GOT_SIGNAL}\"; exit 0' {name}\necho '{READY}'\nwhile :; do sleep 1; done\n"
    ));
    let agentrun = env.start(&[], true, Stdio::null());
    agentrun.wait_for_ready();
    let sent = Instant::now();
    agentrun.signal(signal);
    let outcome = agentrun.finish(&env);
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
    let env = Env::new(&format!("trap '' TERM\necho '{READY}'\nsleep 30\n"));
    let agentrun = env.start(&[], true, Stdio::null());
    agentrun.wait_for_ready();
    let sent = Instant::now();
    agentrun.signal(libc::SIGTERM);
    let outcome = agentrun.finish(&env);
    assert_between(sent.elapsed(), 4.5, 9.0, &outcome.events);
    assert_eq!(outcome.code, 143);
    assert_eq!(outcome.end()["status"], "interrupted");
    assert_eq!(outcome.end()["exit_code"], Value::Null);
}

fn second_signal_kills_immediately() {
    let env = Env::new(&format!("trap '' INT\necho '{READY}'\nsleep 30\n"));
    let agentrun = env.start(&[], true, Stdio::null());
    agentrun.wait_for_ready();
    agentrun.signal(libc::SIGINT);
    thread::sleep(Duration::from_secs(1));
    let sent = Instant::now();
    agentrun.signal(libc::SIGINT);
    let outcome = agentrun.finish(&env);
    assert_between(sent.elapsed(), 0.0, 1.0, &outcome.events);
    assert_eq!(outcome.code, 130);
    assert_eq!(outcome.end()["status"], "interrupted");
    assert_eq!(outcome.end()["exit_code"], Value::Null);
}

fn timeout_terminates_the_runtime() {
    let env = Env::new(&format!("echo '{READY}'\nsleep 30\n"));
    let outcome = env
        .start(&["--timeout", "3"], true, Stdio::null())
        .finish(&env);
    assert_between(outcome.elapsed, 2.9, 5.0, &outcome.events);
    assert_eq!(outcome.code, 3);
    assert_eq!(outcome.end()["status"], "timeout");
    assert_eq!(outcome.end()["exit_code"], Value::Null);
    assert_eq!(outcome.end()["detail"], "");
    assert_eq!(outcome.end()["result"], "ready");
}

fn timeout_with_ignored_sigterm_kills_after_grace() {
    let env = Env::new(&format!("trap '' TERM\necho '{READY}'\nsleep 30\n"));
    let outcome = env
        .start(&["--timeout", "3"], true, Stdio::null())
        .finish(&env);
    assert_between(outcome.elapsed, 7.5, 11.0, &outcome.events);
    assert_eq!(outcome.code, 3);
    assert_eq!(outcome.end()["status"], "timeout");
}

fn signal_during_timeout_grace_kills_immediately() {
    let env = Env::new(&format!("trap '' TERM\necho '{READY}'\nsleep 30\n"));
    let agentrun = env.start(&["--timeout", "3"], true, Stdio::null());
    agentrun.wait_for_ready();
    thread::sleep(Duration::from_secs(4));
    let sent = Instant::now();
    agentrun.signal(libc::SIGINT);
    let outcome = agentrun.finish(&env);
    assert_between(sent.elapsed(), 0.0, 1.0, &outcome.events);
    assert_eq!(outcome.code, 130);
    assert_eq!(outcome.end()["status"], "interrupted");
}

fn signal_before_launch_ends_without_start() {
    let env = Env::new("exit 0\n");
    let mut agentrun = env.start(&[], false, Stdio::piped());
    let mut stdin = agentrun.child.stdin.take().unwrap();
    stdin.write_all(b"partial prompt").unwrap();
    thread::sleep(Duration::from_millis(500));
    agentrun.signal(libc::SIGTERM);
    let outcome = agentrun.finish(&env);
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

#[cfg(target_os = "linux")]
fn signal_during_sandbox_check_kills_the_check() {
    let env = Env::new("exit 0\n");
    env.write_script("bwrap", "echo $$ > \"$PIDFILE\"\nexec /bin/sleep 30\n");
    env.write_script("socat", "exit 0\n");
    let agentrun = env.start(&["--sandbox", "on"], true, Stdio::null());
    let pidfile = env.root.path().join("pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::read_to_string(&pidfile).map_or(true, |text| text.trim().is_empty())
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    let pid = read_pid(&pidfile);
    let sent = Instant::now();
    agentrun.signal(libc::SIGTERM);
    let outcome = agentrun.finish(&env);
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
    let env = Env::new(concat!(
        "echo '{\"record\":\"terminate\",\"detail\":\"model mismatch\"}'\n",
        "sleep 1\n",
        "echo '{\"record\":\"text\",\"parent\":null,\"text\":\"after\"}'\n",
    ));
    let outcome = env.start(&[], true, Stdio::null()).finish(&env);
    assert_between(outcome.elapsed, 0.0, 2.0, &outcome.events);
    assert_eq!(outcome.code, 1);
    assert_eq!(outcome.end()["status"], "failed");
    assert_eq!(outcome.end()["detail"], "model mismatch");
    assert_eq!(outcome.end()["exit_code"], Value::Null);
    assert!(outcome.texts().is_empty(), "{:?}", outcome.texts());
}

fn first_signal_reaches_the_wrapped_runtime_directly() {
    if !sandbox_available() {
        return;
    }
    let env = Env::new(&format!(
        "trap 'sleep 1; : > \"$PWD/trap-ran\"; exit 0' TERM\necho '{READY}'\nwhile :; do sleep 1; done\n"
    ));
    let agentrun = env.start(&["--sandbox", "on"], true, Stdio::null());
    agentrun.wait_for_ready();
    let sent = Instant::now();
    agentrun.signal(libc::SIGTERM);
    let outcome = agentrun.finish(&env);
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
