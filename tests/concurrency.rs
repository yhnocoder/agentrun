#[path = "support/harness.rs"]
mod harness;
#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use support::WebServer;
use support::process::{
    self, FAKE_AGENTRUN, describe, process_is_gone, ps, read_pid, spawn_lock, spawn_role,
    wait_until_gone,
};
use tempfile::TempDir;

const RUNS: usize = 8;
const DRY_RUNS: usize = 10;
const CLEANUP_RUNS: usize = 3;
const LIMIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);
const CREDENTIAL_BYTES: usize = 32 * 1024;
const WAIT_FOR: &str = "wait_for() {\n  i=0\n  while [ ! -e \"$1\" ]; do\n    i=$((i + 1))\n    [ \"$i\" -gt 600 ] && exit 9\n    sleep 0.05\n  done\n}\n";
const TEXT_LINE: &str = "printf '{\"record\":\"text\",\"parent\":null,\"text\":\"%s\"}\\n'";

fn main() {
    process::take_over_if_spawned();
    let tests: Vec<(&str, fn())> = vec![
        (
            "eight_runs_without_sandbox_use_and_remove_their_own_tempdirs",
            eight_runs_without_sandbox_use_and_remove_their_own_tempdirs,
        ),
        (
            "eight_runs_in_the_sandbox_use_and_remove_their_own_tempdirs",
            eight_runs_in_the_sandbox_use_and_remove_their_own_tempdirs,
        ),
        (
            "each_run_reports_only_its_own_network_and_cannot_reach_another_proxy",
            each_run_reports_only_its_own_network_and_cannot_reach_another_proxy,
        ),
        (
            "cleanup_of_one_run_leaves_the_other_runs_alone",
            cleanup_of_one_run_leaves_the_other_runs_alone,
        ),
        (
            "concurrent_credential_writes_leave_a_consistent_login_file",
            concurrent_credential_writes_leave_a_consistent_login_file,
        ),
        (
            "ten_sandbox_checks_at_once_all_pass",
            ten_sandbox_checks_at_once_all_pass,
        ),
    ];
    harness::run_tests(tests);
}

struct Env {
    root: TempDir,
}

impl Env {
    fn new() -> Env {
        let env = Env {
            root: tempfile::tempdir().unwrap(),
        };
        for dir in [env.bin(), env.tmp(), env.home()] {
            std::fs::create_dir(dir).unwrap();
        }
        env
    }

    fn install(&self, names: &[&str], script: &str) {
        let _guard = spawn_lock();
        for name in names {
            let path = self.bin().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{WAIT_FOR}{script}")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    fn tmp(&self) -> PathBuf {
        self.root.path().join("tmp")
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn work(&self, index: usize) -> PathBuf {
        let work = self.root.path().join(format!("work-{index}"));
        std::fs::create_dir_all(&work).unwrap();
        work
    }

    fn agentrun_dirs(&self) -> BTreeSet<String> {
        std::fs::read_dir(self.tmp())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("agentrun-"))
            .collect()
    }

    fn start(
        &self,
        runtime: &str,
        work: &Path,
        extra: &[&str],
        variables: &[(&str, String)],
    ) -> Agentrun {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.arg(runtime);
        if !extra.contains(&"--sandbox") {
            command.args(["--sandbox", "off"]);
        }
        command
            .arg("--cwd")
            .arg(work)
            .args(["--prompt", "hi"])
            .args(extra)
            .env_clear()
            .env(FAKE_AGENTRUN, "1")
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            .env("TMPDIR", self.tmp())
            .env("HOME", self.home())
            .envs(variables.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = {
            let _guard = spawn_lock();
            command.spawn().unwrap()
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&events);
        let stdout = child.stdout.take().unwrap();
        let stdout_reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                let event =
                    serde_json::from_str(&line).unwrap_or_else(|_| Value::String(line.clone()));
                collected.lock().unwrap().push(event);
            }
        });
        let mut stderr = child.stderr.take().unwrap();
        let stderr_reader = thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        Agentrun {
            child,
            events,
            readers: Some((stdout_reader, stderr_reader)),
        }
    }
}

type Readers = (thread::JoinHandle<()>, thread::JoinHandle<String>);

struct Agentrun {
    child: Child,
    events: Arc<Mutex<Vec<Value>>>,
    readers: Option<Readers>,
}

struct Finished {
    code: i32,
    events: Vec<Value>,
    stderr: String,
}

impl Finished {
    fn status(&self) -> &Value {
        &self.events.last().unwrap()["status"]
    }

    fn texts(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|event| event["type"] == "text")
            .map(|event| event["text"].as_str().unwrap().to_string())
            .collect()
    }

    fn networks(&self) -> Vec<(String, u64, bool)> {
        self.events
            .iter()
            .filter(|event| event["type"] == "network")
            .map(|event| {
                (
                    event["host"].as_str().unwrap().to_string(),
                    event["port"].as_u64().unwrap(),
                    event["allowed"].as_bool().unwrap(),
                )
            })
            .collect()
    }
}

impl Agentrun {
    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    fn signal(&self, signal: i32) {
        unsafe {
            libc::kill(self.pid(), signal);
        }
    }

    fn finish(mut self) -> Finished {
        let deadline = Instant::now() + LIMIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "agentrun {} did not finish within {LIMIT:?}; events {:?}",
                    self.pid(),
                    self.events.lock().unwrap()
                );
            }
            thread::sleep(POLL);
        };
        let (stdout_reader, stderr_reader) = self.readers.take().unwrap();
        stdout_reader.join().unwrap();
        let stderr = stderr_reader.join().unwrap();
        let events = self.events.lock().unwrap().clone();
        assert_eq!(
            events.last().map(|event| &event["type"]),
            Some(&Value::from("end")),
            "{events:?}\n{stderr}"
        );
        Finished {
            code: status.code().unwrap(),
            events,
            stderr,
        }
    }
}

fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(POLL);
    }
}

fn touch(path: &Path) {
    std::fs::write(path, "").unwrap();
}

fn sandbox_available() -> bool {
    let _guard = spawn_lock();
    support::sandbox_available()
}

fn eight_runs_without_sandbox_use_and_remove_their_own_tempdirs() {
    runs_use_and_remove_their_own_tempdirs("off");
}

fn eight_runs_in_the_sandbox_use_and_remove_their_own_tempdirs() {
    if sandbox_available() {
        runs_use_and_remove_their_own_tempdirs("on");
    }
}

fn runs_use_and_remove_their_own_tempdirs(sandbox: &str) {
    let env = Env::new();
    env.install(
        &["claude", "codex"],
        &format!(
            ": > \"$PWD/ready\"\nwait_for \"$GO\"\n{TEXT_LINE} \"$TMPDIR\"\nif [ -d \"$TMPDIR-codex\" ]; then {TEXT_LINE} 'codex home'; fi\n"
        ),
    );
    std::fs::create_dir(env.home().join(".codex")).unwrap();
    std::fs::write(env.home().join(".codex/auth.json"), "{}").unwrap();
    let go = env.path("go");
    let before = env.agentrun_dirs();
    let works: Vec<PathBuf> = (0..RUNS).map(|index| env.work(index)).collect();
    let runtimes: Vec<&str> = (0..RUNS)
        .map(|index| {
            if index % 2 == 0 {
                "claude-code"
            } else {
                "codex"
            }
        })
        .collect();
    let runs: Vec<Agentrun> = works
        .iter()
        .zip(&runtimes)
        .map(|(work, runtime)| {
            env.start(
                runtime,
                work,
                &["--sandbox", sandbox],
                &[("GO", go.display().to_string())],
            )
        })
        .collect();
    wait_until("every fake runtime to start", || {
        works.iter().all(|work| work.join("ready").exists())
    });
    let during = env.agentrun_dirs();
    assert_eq!(during.len(), RUNS + RUNS / 2, "{during:?}");
    touch(&go);
    let mut tempdirs = BTreeSet::new();
    for (run, runtime) in runs.into_iter().zip(&runtimes) {
        let finished = run.finish();
        assert_eq!(
            finished.code, 0,
            "{:?}\n{}",
            finished.events, finished.stderr
        );
        assert_eq!(finished.status(), "finished");
        let texts = finished.texts();
        let tempdir = PathBuf::from(&texts[0]);
        assert_eq!(tempdir.parent(), Some(env.tmp().as_path()), "{texts:?}");
        assert!(tempdirs.insert(tempdir), "{texts:?}");
        assert_eq!(texts.len() == 2, *runtime == "codex", "{texts:?}");
    }
    assert_eq!(tempdirs.len(), RUNS);
    assert_eq!(env.agentrun_dirs(), before);
}

fn each_run_reports_only_its_own_network_and_cannot_reach_another_proxy() {
    if !sandbox_available() {
        return;
    }
    let env = Env::new();
    env.install(
        &["claude"],
        "printf '%s' \"$http_proxy\" > \"$PWD/proxy.txt\"\ncurl -s -m 5 -o /dev/null -w '%{http_code}' \"http://127.0.0.1:$TARGET/\" > \"$PWD/own.txt\"\nwait_for \"$PWD/peer.txt\"\ncurl -s -m 5 -o /dev/null -w '%{http_code}' -x \"$(cat \"$PWD/peer.txt\")\" \"http://127.0.0.1:$TARGET/\" > \"$PWD/cross.txt\"\nexit 0\n",
    );
    let servers = [WebServer::start(), WebServer::start()];
    let works = [env.work(0), env.work(1)];
    let runs: Vec<Agentrun> = servers
        .iter()
        .zip(&works)
        .map(|(server, work)| {
            let target = format!("127.0.0.1:{}", server.port);
            env.start(
                "claude-code",
                work,
                &[
                    "--sandbox",
                    "on",
                    "--network",
                    "custom",
                    "--allow-host",
                    &target,
                ],
                &[("TARGET", server.port.to_string())],
            )
        })
        .collect();
    wait_until("both fake runtimes to write their proxy address", || {
        works.iter().all(|work| {
            std::fs::read_to_string(work.join("own.txt")).is_ok_and(|code| !code.is_empty())
        })
    });
    let proxies: Vec<String> = works
        .iter()
        .map(|work| std::fs::read_to_string(work.join("proxy.txt")).unwrap())
        .collect();
    assert_ne!(proxies[0], proxies[1]);
    std::fs::write(works[0].join("peer.txt"), &proxies[1]).unwrap();
    std::fs::write(works[1].join("peer.txt"), &proxies[0]).unwrap();
    for ((run, work), server) in runs.into_iter().zip(&works).zip(&servers) {
        let finished = run.finish();
        assert_eq!(
            finished.code, 0,
            "{:?}\n{}",
            finished.events, finished.stderr
        );
        assert_eq!(finished.events[0]["sandbox"], support::SANDBOX_KIND);
        let read = |name: &str| std::fs::read_to_string(work.join(name)).unwrap();
        assert_eq!(read("own.txt"), "200");
        assert_eq!(read("cross.txt"), "000");
        assert_eq!(
            finished.networks(),
            vec![("127.0.0.1".to_string(), u64::from(server.port), true)]
        );
    }
    for server in servers {
        assert_eq!(server.served(), 1);
    }
}

struct Spawned {
    group: i32,
    detached: i32,
}

impl Spawned {
    fn read(work: &Path) -> Spawned {
        Spawned {
            group: read_pid(&work.join("group.pid")),
            detached: read_pid(&work.join("detached.pid")),
        }
    }

    fn pids(&self) -> [i32; 2] {
        [self.group, self.detached]
    }

    fn assert_gone(&self) {
        for pid in self.pids() {
            assert!(
                wait_until_gone(pid),
                "{pid} is still running: {}",
                describe(pid)
            );
        }
    }

    fn assert_running(&self) {
        for pid in self.pids() {
            assert!(!process_is_gone(pid), "{pid} was killed");
        }
    }
}

fn cleanup_of_one_run_leaves_the_other_runs_alone() {
    let env = Env::new();
    env.install(
        &["claude"],
        &format!(
            "trap 'exit 0' INT\n{}{}: > \"$PWD/ready\"\nwait_for \"$PWD/exit\"\nexit 0\n",
            spawn_role("sleeper", "$PWD/group.pid"),
            spawn_role("detached", "$PWD/detached.pid"),
        ),
    );
    let works: Vec<PathBuf> = (0..CLEANUP_RUNS).map(|index| env.work(index)).collect();
    let mut runs: Vec<Option<Agentrun>> = works
        .iter()
        .map(|work| Some(env.start("claude-code", work, &[], &[])))
        .collect();
    wait_until("every fake runtime to start its processes", || {
        works.iter().all(|work| work.join("ready").exists())
    });
    let spawned: Vec<Spawned> = works.iter().map(|work| Spawned::read(work)).collect();
    for run in &spawned {
        run.assert_running();
    }

    let interrupted = runs[0].take().unwrap();
    interrupted.signal(libc::SIGINT);
    let finished = interrupted.finish();
    assert_eq!(finished.code, 130, "{:?}", finished.events);
    assert_eq!(finished.status(), "interrupted");
    spawned[0].assert_gone();
    spawned[1].assert_running();
    spawned[2].assert_running();

    touch(&works[1].join("exit"));
    let finished = runs[1].take().unwrap().finish();
    assert_eq!(finished.code, 0, "{:?}", finished.events);
    assert_eq!(finished.status(), "finished");
    spawned[1].assert_gone();
    spawned[2].assert_running();

    touch(&works[2].join("exit"));
    let finished = runs[2].take().unwrap().finish();
    assert_eq!(finished.code, 0, "{:?}", finished.events);
    spawned[2].assert_gone();

    let marker = env.root.path().display().to_string();
    let left: Vec<String> = ps(&["-A", "-o", "pid=,args="])
        .lines()
        .filter(|line| line.contains(&marker))
        .map(str::to_string)
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

fn concurrent_credential_writes_leave_a_consistent_login_file() {
    let env = Env::new();
    env.install(&["pi"], &format!("{TEXT_LINE} done\n"));
    let login = env.home().join(".pi/agent/auth.json");
    let value = |seed: usize| -> String {
        let letter = char::from(b'a' + seed as u8);
        format!(
            "{{\"key\":\"{}\"}}",
            letter.to_string().repeat(CREDENTIAL_BYTES)
        )
    };
    let same: Vec<String> = vec![value(0); RUNS];
    let different: Vec<String> = (1..=RUNS).map(value).collect();
    for values in [same, different] {
        let mut readable = values.clone();
        readable.extend(std::fs::read_to_string(&login).ok());
        let watching = Arc::new(AtomicBool::new(true));
        let watcher = {
            let watching = Arc::clone(&watching);
            let login = login.clone();
            thread::spawn(move || {
                let mut reads = 0;
                while watching.load(Ordering::SeqCst) {
                    if let Ok(content) = std::fs::read_to_string(&login) {
                        assert!(
                            readable.contains(&content),
                            "read a value that no run passed: {} bytes",
                            content.len()
                        );
                        reads += 1;
                    }
                }
                reads
            })
        };
        let runs: Vec<Agentrun> = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                env.start(
                    "pi",
                    &env.work(index),
                    &[],
                    &[("AGENTRUN_PI_AUTH", value.clone())],
                )
            })
            .collect();
        for run in runs {
            let finished = run.finish();
            assert_eq!(
                finished.code, 0,
                "{:?}\n{}",
                finished.events, finished.stderr
            );
        }
        watching.store(false, Ordering::SeqCst);
        assert!(watcher.join().unwrap() > 0);
        let content = std::fs::read_to_string(&login).unwrap();
        assert!(values.contains(&content));
        let digest: String = Sha256::digest(content.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            std::fs::read_to_string(login.with_file_name("auth.json.agentrun-sha256")).unwrap(),
            format!("{digest}\n"),
            "the hash file does not match the login file"
        );
        let mut names: Vec<String> = std::fs::read_dir(login.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["auth.json", "auth.json.agentrun-sha256"]);
    }
}

fn ten_sandbox_checks_at_once_all_pass() {
    if !sandbox_available() {
        return;
    }
    let env = Env::new();
    env.install(&["claude"], "exit 0\n");
    let runs: Vec<Agentrun> = (0..DRY_RUNS)
        .map(|index| {
            env.start(
                "claude-code",
                &env.work(index),
                &["--sandbox", "on", "--dry-run", "--debug"],
                &[],
            )
        })
        .collect();
    let mut checks = Vec::new();
    for run in runs {
        let deadline = Instant::now() + LIMIT;
        let mut run = run;
        let status = loop {
            if let Some(status) = run.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "a dry run did not finish");
            thread::sleep(POLL);
        };
        let (stdout_reader, stderr_reader) = run.readers.take().unwrap();
        stdout_reader.join().unwrap();
        let stderr = stderr_reader.join().unwrap();
        let events = run.events.lock().unwrap().clone();
        assert_eq!(status.code(), Some(0), "{events:?}\n{stderr}");
        assert!(
            events[0]
                .as_str()
                .is_some_and(|line| line.starts_with("command: ")),
            "{events:?}"
        );
        let check = stderr
            .lines()
            .find_map(|line| line.strip_prefix("[debug] sandbox: "))
            .and_then(|line| line.rsplit_once("check "))
            .map(|(_, rest)| rest.trim_end_matches(')').to_string())
            .unwrap_or_else(|| panic!("no sandbox check time in {stderr}"));
        checks.push(check);
    }
    println!(
        "sandbox checks of {DRY_RUNS} concurrent dry runs: {}",
        checks.join(" ")
    );
}
