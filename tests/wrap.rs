use std::io::Read;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentrun::sandbox::{create_pi_state_dir, wrap_pi, wrapper_failure};
use tempfile::TempDir;

struct Wrapped {
    root: TempDir,
    bwrap: PathBuf,
}

impl Wrapped {
    fn new() -> Option<Wrapped> {
        let bwrap = ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.exists());
        let Some(bwrap) = bwrap else {
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

    fn script(&self, body: &str) -> PathBuf {
        let path = self.cwd().join("script.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn command(&self, body: &str, args: &[&str]) -> Command {
        create_pi_state_dir(&self.state_dir()).unwrap();
        let script = self.script(body);
        let mut argv = vec![script.to_string_lossy().into_owned()];
        argv.extend(args.iter().map(|arg| arg.to_string()));
        let wrapped = wrap_pi(
            &self.bwrap,
            &self.cwd(),
            &self.tempdir(),
            &self.state_dir(),
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
fn writes_only_cwd_tempdir_and_state_dir() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    let other_tmp = PathBuf::from(format!(
        "/tmp/agentrun-wrap-test-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let output = wrapped.run(
        "for f in \"$@\"; do if echo probe > \"$f\" 2>/dev/null; then echo \"written: $f\"; else echo \"blocked: $f\"; fi; done\n",
        &[
            wrapped.cwd().join("inside.txt").to_str().unwrap(),
            wrapped.tempdir().join("temp.txt").to_str().unwrap(),
            wrapped.state_dir().join("state.txt").to_str().unwrap(),
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
        ["written", "written", "written", "blocked", "blocked"]
    );
    assert!(wrapped.cwd().join("inside.txt").exists());
    assert!(wrapped.tempdir().join("temp.txt").exists());
    assert!(wrapped.state_dir().join("state.txt").exists());
    assert!(!wrapped.home().join("outside.txt").exists());
    assert!(!other_tmp.exists());
}

#[test]
fn state_dir_is_created_before_wrapping() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    assert!(!wrapped.state_dir().exists());
    let output = wrapped.run("echo ok\n", &[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    for created in [wrapped.home().join(".pi"), wrapped.state_dir()] {
        let mode = std::fs::metadata(&created).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", created.display());
    }
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
    assert_eq!(wrapper_failure(output.status.code(), ""), None);
}

fn process_is_gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(status) => status.lines().any(|line| line.starts_with("State:\tZ")),
        Err(_) => true,
    }
}

#[test]
fn sigterm_to_the_process_group_ends_the_wrapped_command() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    let pidfile = wrapped.cwd().join("pid");
    let mut command = wrapped.command(
        "trap '' TERM\necho $$ > \"$1\"\necho ready\nwhile :; do sleep 1; done\n",
        &[pidfile.to_str().unwrap()],
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
    let deadline = Instant::now() + Duration::from_secs(2);
    while !process_is_gone(script_pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        process_is_gone(script_pid),
        "the wrapped script {script_pid} is still running"
    );
}

#[test]
fn wrapper_failure_is_recognised_from_a_real_bwrap_error() {
    let Some(wrapped) = Wrapped::new() else {
        return;
    };
    let missing = wrapped.root.path().join("missing");
    let argv = vec!["/bin/true".to_string()];
    let bad = wrap_pi(
        &wrapped.bwrap,
        &wrapped.cwd(),
        &wrapped.tempdir(),
        &missing,
        &argv,
    );
    let output = Command::new(&bad[0]).args(&bad[1..]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = wrapper_failure(output.status.code(), &stderr).unwrap();
    assert!(detail.starts_with("sandbox failed to start: "), "{detail}");
    assert!(detail.contains("missing"), "{detail}");
    assert!(Path::new(&bad[0]).is_absolute());
}
