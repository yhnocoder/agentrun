#![cfg(target_os = "linux")]

#[allow(dead_code)]
mod support;

use std::io::{BufRead, Read};
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentrun::event::SandboxKind;
use agentrun::pi::state;
use agentrun::sandbox::{PiState, ProxyForward, wrap_pi, wrapper_failure};
use support::env::write_script;
use support::process::wait_until_gone;
use tempfile::TempDir;

struct Wrapped {
    root: TempDir,
    bwrap: PathBuf,
}

impl Wrapped {
    fn new() -> Option<Wrapped> {
        let Some(bwrap) = support::system_bwrap() else {
            eprintln!("skipped: bwrap is not installed");
            return None;
        };
        let probe = Command::new(&bwrap)
            .args([
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--die-with-parent",
                "--",
                "/bin/true",
            ])
            .output()
            .ok()?;
        if !probe.status.success() {
            eprintln!(
                "skipped: bwrap cannot start here: {}",
                String::from_utf8_lossy(&probe.stderr).trim()
            );
            return None;
        }
        let wrapped = Wrapped {
            root: tempfile::tempdir().unwrap(),
            bwrap,
        };
        for dir in [wrapped.cwd(), wrapped.tempdir(), wrapped.home()] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::create_dir_all(wrapped.state_dir()).unwrap();
        std::fs::write(wrapped.state_dir().join("auth.json"), "{}").unwrap();
        std::fs::write(wrapped.state_dir().join("settings.json"), "{}").unwrap();
        Some(wrapped)
    }

    fn cwd(&self) -> PathBuf {
        self.root.path().join("work")
    }

    fn tempdir(&self) -> PathBuf {
        self.root.path().join("agentrun-session")
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn state_dir(&self) -> PathBuf {
        self.home().join(".pi/agent")
    }

    fn real_state_dir(&self) -> PathBuf {
        std::fs::canonicalize(self.state_dir()).unwrap()
    }

    fn script(&self, body: &str) -> PathBuf {
        let path = self.cwd().join("script.sh");
        write_script(&path, &format!("#!/bin/sh\n{body}"));
        path
    }

    fn command(&self, body: &str, args: &[&str]) -> Command {
        self.command_with_forward(body, args, None)
    }

    fn command_with_forward(
        &self,
        body: &str,
        args: &[&str],
        forward: Option<&ProxyForward>,
    ) -> Command {
        let state = state(&self.state_dir()).unwrap();
        let script = self.script(body);
        let mut argv = vec![script.to_string_lossy().into_owned()];
        argv.extend(args.iter().map(|arg| arg.to_string()));
        let wrapped = wrap_pi(
            &self.bwrap,
            &self.cwd(),
            &self.tempdir(),
            Some(&state),
            forward,
            &argv,
        );
        let mut command = Command::new(&wrapped[0]);
        command
            .args(&wrapped[1..])
            .current_dir(self.cwd())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.home())
            .env("TMPDIR", self.tempdir())
            .stdin(Stdio::null());
        command
    }

    fn run(&self, body: &str, args: &[&str]) -> Output {
        self.command(body, args).output().unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn writes_only_cwd_tempdir_and_the_writable_state_entries() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    let other_tmp = PathBuf::from(format!(
        "/tmp/agentrun-wrap-test-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let state_dir = wrapped.real_state_dir();
    let output = wrapped.run(
        "for f in \"$@\"; do if echo probe > \"$f\" 2>/dev/null; then echo \"written: $f\"; else echo \"blocked: $f\"; fi; done\n",
        &[
            wrapped.cwd().join("inside.txt").to_str().unwrap(),
            wrapped.tempdir().join("temp.txt").to_str().unwrap(),
            state_dir.join("auth.json").to_str().unwrap(),
            state_dir.join("models-store.json").to_str().unwrap(),
            state_dir.join("settings.json").to_str().unwrap(),
            wrapped.home().join("outside.txt").to_str().unwrap(),
            other_tmp.to_str().unwrap(),
        ],
    );
    let _ = std::fs::remove_file(&other_tmp);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    let lines: Vec<&str> = text
        .lines()
        .map(|line| line.split(':').next().unwrap())
        .collect();
    assert_eq!(
        lines,
        [
            "written", "written", "written", "written", "blocked", "blocked", "blocked"
        ]
    );
    assert!(wrapped.cwd().join("inside.txt").exists());
    assert!(wrapped.tempdir().join("temp.txt").exists());
    assert_eq!(
        std::fs::read_to_string(state_dir.join("auth.json")).unwrap(),
        "probe\n"
    );
    assert_eq!(
        std::fs::read_to_string(state_dir.join("models-store.json")).unwrap(),
        "probe\n"
    );
    assert_eq!(
        std::fs::read_to_string(state_dir.join("settings.json")).unwrap(),
        "{}"
    );
    assert!(!wrapped.home().join("outside.txt").exists());
    assert!(!other_tmp.exists());
}

#[test]
fn lock_directories_can_be_created_and_removed_in_the_state_dir() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    let state_dir = wrapped.real_state_dir();
    let output = wrapped.run(
        "for d in \"$@\"; do if mkdir \"$d\" 2>/dev/null; then echo \"created: $d\"; rmdir \"$d\" && echo \"removed: $d\"; else echo \"blocked: $d\"; fi; done\n",
        &[
            state_dir.join("auth.json.lock").to_str().unwrap(),
            state_dir.join("models-store.json.lock").to_str().unwrap(),
            state_dir.join("settings.json.lock").to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    let lines: Vec<&str> = text
        .lines()
        .map(|line| line.split(':').next().unwrap())
        .collect();
    assert_eq!(lines, ["created", "removed"].repeat(3));
    assert!(!state_dir.join("auth.json.lock").exists());
}

#[test]
fn new_entries_in_the_state_dir_are_a_remaining_risk() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    let state_dir = wrapped.real_state_dir();
    let output = wrapped.run(
        "mkdir \"$1\" && echo created\n",
        &[state_dir.join("extensions").to_str().unwrap()],
    );
    assert_eq!(stdout(&output).trim(), "created", "{output:?}");
    assert!(state_dir.join("extensions").is_dir());
}

#[test]
fn no_network_inside_the_wrapper() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if listener.accept().is_ok() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    });
    let output = wrapped.run(
        "if socat -T 2 - \"TCP:127.0.0.1:$1\" </dev/null 2>/dev/null; then echo connected; else echo refused; fi\n",
        &[&port.to_string()],
    );
    assert_eq!(stdout(&output).trim(), "refused");
    assert!(
        !accepted.join().unwrap(),
        "the sandboxed script reached the listener"
    );
}

#[test]
fn tmpdir_inside_is_the_session_tempdir() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    let output = wrapped.run("echo \"$TMPDIR\"\n", &[]);
    assert_eq!(stdout(&output).trim(), wrapped.tempdir().to_string_lossy());
}

#[test]
fn exit_code_passes_through() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    let output = wrapped.run("exit 7\n", &[]);
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        wrapper_failure(SandboxKind::Bubblewrap, output.status.code(), ""),
        None
    );
}

#[test]
fn sigterm_to_the_process_group_kills_the_wrapped_command_before_its_trap() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    let pidfile = wrapped.cwd().join("pid");
    let marker = wrapped.cwd().join("trap-ran");
    let mut command = wrapped.command(
        "trap 'sleep 1; : > \"$2\"; exit 0' TERM\necho $$ > \"$1\"\necho ready\nwhile :; do sleep 1; done\n",
        &[pidfile.to_str().unwrap(), marker.to_str().unwrap()],
    );
    let mut child = command
        .process_group(0)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = [0u8; 6];
    child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready\n");
    let script_pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let sent = Instant::now();
    unsafe {
        libc::killpg(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().unwrap();
    assert!(
        sent.elapsed() < Duration::from_secs(2),
        "{:?}",
        sent.elapsed()
    );
    assert_eq!(status.code(), None, "{status:?}");
    assert!(
        wait_until_gone(script_pid),
        "the wrapped script {script_pid} is still running"
    );
    thread::sleep(Duration::from_millis(1500));
    assert!(!marker.exists(), "the trap of the wrapped script ran");
}

#[test]
fn wrapper_failure_is_recognised_from_a_real_bwrap_error() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    let missing = PiState {
        dir: wrapped.root.path().join("missing"),
        login_target: None,
        readonly: Vec::new(),
    };
    let argv = vec!["/bin/true".to_string()];
    let bad = wrap_pi(
        &wrapped.bwrap,
        &wrapped.cwd(),
        &wrapped.tempdir(),
        Some(&missing),
        None,
        &argv,
    );
    let output = Command::new(&bad[0]).args(&bad[1..]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = wrapper_failure(SandboxKind::Bubblewrap, output.status.code(), &stderr).unwrap();
    assert!(detail.starts_with("sandbox failed to start: "), "{detail}");
    assert!(detail.contains("missing"), "{detail}");
    assert!(Path::new(&bad[0]).is_absolute());
}

#[test]
fn forward_connects_the_sandbox_port_to_the_unix_socket() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    let socket = wrapped.tempdir().join("proxy.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let echo = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 4];
        stream.read_exact(&mut request).unwrap();
        std::io::Write::write_all(&mut stream, b"pong").unwrap();
        request
    });
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
        .to_string();
    let forward = ProxyForward {
        socat: Path::new("/usr/bin/socat"),
        port: &port,
        socket: &socket,
    };
    let mut child = wrapped
        .command_with_forward(
            "for i in 1 2 3 4 5 6 7 8 9 10; do out=$(printf ping | socat -T 2 - \"TCP:127.0.0.1:$1\" 2>/dev/null) && break; sleep 0.1; done; echo \"$out\"\n",
            &[&port],
            Some(&forward),
        )
        .process_group(0)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    unsafe {
        libc::killpg(child.id() as i32, libc::SIGKILL);
    }
    child.wait().unwrap();
    assert_eq!(line.trim(), "pong");
    assert_eq!(&echo.join().unwrap(), b"ping");
}
