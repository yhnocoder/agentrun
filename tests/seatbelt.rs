#![cfg(target_os = "macos")]

#[allow(dead_code)]
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use agentrun::event::SandboxKind;
use agentrun::sandbox::{wrap_seatbelt, wrapper_failure, write_seatbelt_profile};
use tempfile::TempDir;

struct Wrapped {
    root: TempDir,
}

impl Wrapped {
    fn new() -> Wrapped {
        let wrapped = Wrapped {
            root: tempfile::tempdir().unwrap(),
        };
        for dir in [wrapped.cwd(), wrapped.tempdir(), wrapped.home()] {
            std::fs::create_dir(dir).unwrap();
        }
        wrapped
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

    fn login_file(&self) -> PathBuf {
        std::fs::canonicalize(self.home())
            .unwrap()
            .join("auth.json")
    }

    fn run(&self, body: &str, args: &[&str], port: Option<&str>) -> Output {
        std::fs::write(self.login_file(), "{}").unwrap();
        write_seatbelt_profile(&self.cwd(), &self.tempdir(), Some(&self.login_file()), port)
            .unwrap();
        let script = self.tempdir().join("script.sh");
        std::fs::write(&script, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut argv = vec![script.to_string_lossy().into_owned()];
        argv.extend(args.iter().map(|arg| arg.to_string()));
        let wrapped = wrap_seatbelt(&self.tempdir(), &argv);
        Command::new(&wrapped[0])
            .args(&wrapped[1..])
            .current_dir(self.cwd())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.home())
            .env("TMPDIR", self.tempdir())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

const WRITE_EACH: &str = "for f in \"$@\"; do if echo probe > \"$f\" 2>/dev/null; then echo \"written: $f\"; else echo \"blocked: $f\"; fi; done\n";

#[test]
fn writes_only_cwd_tempdir_and_login_file() {
    let wrapped = Wrapped::new();
    let other_tmp = PathBuf::from(format!(
        "/tmp/agentrun-seatbelt-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let output = wrapped.run(
        WRITE_EACH,
        &[
            wrapped.cwd().join("inside.txt").to_str().unwrap(),
            wrapped.tempdir().join("temp.txt").to_str().unwrap(),
            wrapped.login_file().to_str().unwrap(),
            wrapped.home().join("outside.txt").to_str().unwrap(),
            other_tmp.to_str().unwrap(),
        ],
        None,
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
    assert_eq!(
        std::fs::read_to_string(wrapped.login_file()).unwrap(),
        "probe\n"
    );
    assert!(!wrapped.home().join("outside.txt").exists());
    assert!(!other_tmp.exists());
}

#[test]
fn paths_with_quotes_and_backslashes_stay_writable() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work \"q\" \\b");
    let tempdir = root.path().join("session");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&tempdir).unwrap();
    write_seatbelt_profile(&cwd, &tempdir, None, None).unwrap();
    let argv = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "echo x > \"$0/inside.txt\" && echo x > \"$0/../outside.txt\"".to_string(),
        cwd.to_string_lossy().into_owned(),
    ];
    let command = wrap_seatbelt(&tempdir, &argv);
    let output = Command::new(&command[0])
        .args(&command[1..])
        .output()
        .unwrap();
    assert!(cwd.join("inside.txt").exists(), "{output:?}");
    assert!(!root.path().join("outside.txt").exists(), "{output:?}");
}

#[test]
fn child_and_grandchild_processes_are_restricted_too() {
    let wrapped = Wrapped::new();
    let outside = wrapped.home().join("child.txt");
    let inside = wrapped.cwd().join("child.txt");
    let output = wrapped.run(
        "/bin/sh -c '/bin/sh -c \"echo x > $1; echo x > $2\" _ \"$1\" \"$2\"' _ \"$1\" \"$2\"\n",
        &[outside.to_str().unwrap(), inside.to_str().unwrap()],
        None,
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Operation not permitted"),
        "{output:?}"
    );
    assert!(!outside.exists());
    assert!(inside.exists());
}

const CURL: &str = "for port in \"$@\"; do code=$(curl -sS -m 3 -o /dev/null -w '%{http_code}' \"http://127.0.0.1:$port/\" 2>/dev/null); echo \"$port=$code\"; done\n";

#[test]
fn network_reaches_only_the_port_in_the_profile() {
    let wrapped = Wrapped::new();
    let allowed = support::WebServer::start();
    let denied = support::WebServer::start();
    let allowed_port = allowed.port.to_string();
    let denied_port = denied.port.to_string();
    let output = wrapped.run(CURL, &[&allowed_port, &denied_port], Some(&allowed_port));
    assert_eq!(
        stdout(&output),
        format!("{allowed_port}=200\n{denied_port}=000\n"),
        "{output:?}"
    );
    assert_eq!(allowed.served(), 1);
    assert_eq!(denied.served(), 0);
}

#[test]
fn without_a_port_no_connection_is_allowed() {
    let wrapped = Wrapped::new();
    let server = support::WebServer::start();
    let port = server.port.to_string();
    let output = wrapped.run(CURL, &[&port], None);
    assert_eq!(stdout(&output), format!("{port}=000\n"), "{output:?}");
    assert_eq!(server.served(), 0);
}

#[test]
fn the_wrapped_command_keeps_the_process_id_and_exit_code() {
    let wrapped = Wrapped::new();
    write_seatbelt_profile(&wrapped.cwd(), &wrapped.tempdir(), None, None).unwrap();
    let argv = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "echo $$; exit 7".to_string(),
    ];
    let command = wrap_seatbelt(&wrapped.tempdir(), &argv);
    let child = Command::new(&command[0])
        .args(&command[1..])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let output = child.wait_with_output().unwrap();
    assert_eq!(stdout(&output).trim(), pid.to_string());
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        wrapper_failure(SandboxKind::Seatbelt, output.status.code(), ""),
        None
    );
}

#[test]
fn wrapper_failure_is_recognised_from_a_real_sandbox_exec_error() {
    let wrapped = Wrapped::new();
    let argv = vec!["/usr/bin/true".to_string()];
    let command = wrap_seatbelt(&wrapped.tempdir(), &argv);
    let output = Command::new(&command[0])
        .args(&command[1..])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = wrapper_failure(SandboxKind::Seatbelt, output.status.code(), &stderr).unwrap();
    assert!(detail.starts_with("sandbox failed to start: "), "{detail}");
    assert!(detail.contains("seatbelt.sb"), "{detail}");
}
