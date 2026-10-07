pub mod env;
pub mod fake;
pub mod process;

use std::fs::File;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Instant;

use agentrun::cli::{Cli, Parsed, Runtime, SandboxMode};
use agentrun::output::{Aggregator, SandboxKind};
use agentrun::run::{Exit, conclude, stderr_tail, translate_line};
use agentrun::runtime::{Adapter, Invocation};
use agentrun::sandbox::{Sandbox, Wrapper};
use agentrun::session::Session;
use clap::Parser;
use serde_json::Value;

pub struct Meta {
    pub cwd: PathBuf,
    pub exit_code: Option<i32>,
    pub stderr: String,
}

pub fn read_meta(path: &Path) -> Option<Meta> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).expect("meta fixture is JSON");
    Some(Meta {
        cwd: PathBuf::from(value["cwd"].as_str().expect("meta has cwd")),
        exit_code: value["exit_code"].as_i64().map(|code| code as i32),
        stderr: value["stderr"].as_str().unwrap_or_default().to_string(),
    })
}

pub fn invocation(runtime: Runtime, cwd: &Path, prompt: &str, sandboxed: bool) -> Invocation {
    let cli = Cli::try_parse_from(["agentrun", runtime.name(), "--prompt", prompt]).unwrap();
    let Parsed::Run(runtime, args) = cli.command.into_parsed() else {
        unreachable!("the tests parse runtime subcommands");
    };
    let wrapper = if sandboxed {
        Wrapper::Bubblewrap {
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            socat: PathBuf::from("/usr/bin/socat"),
        }
    } else {
        Wrapper::None
    };
    Invocation {
        sandbox: Sandbox::new(SandboxMode::On, wrapper),
        ..Invocation::new(
            runtime,
            args,
            cwd.to_path_buf(),
            prompt.to_string(),
            Session::assemble(runtime, &[], &[], &[], &[]),
        )
    }
}

pub fn replay(adapter: &mut dyn Adapter, raw: &Path, prompt: &str) -> Vec<Value> {
    let started = Instant::now();
    let content = std::fs::read(raw).expect("raw fixture is readable");
    let meta = read_meta(
        &raw.with_file_name(
            raw.file_name()
                .unwrap()
                .to_string_lossy()
                .replace(".raw.jsonl", ".meta.json"),
        ),
    );
    let tempdir = tempfile::tempdir().expect("replay tempdir");
    let (exit, stderr, wrapped) = match &meta {
        Some(meta) => {
            let runtime = adapter.runtime();
            let mut invocation = invocation(runtime, &meta.cwd, prompt, true);
            invocation.tempdir = tempdir.path().to_path_buf();
            invocation.args.dry_run = true;
            if runtime == Runtime::Codex {
                invocation.session.codex_auth = Some("{}".into());
            }
            let launch = adapter
                .launch(Path::new(runtime.executable()), &invocation)
                .unwrap();
            let exit = Exit {
                code: meta.exit_code,
                signal: None,
                timed_out: false,
            };
            (exit, stderr_tail(&meta.stderr), launch.wrapped)
        }
        None => {
            let exit = Exit {
                code: Some(0),
                signal: None,
                timed_out: false,
            };
            (exit, String::new(), SandboxKind::None)
        }
    };
    let mut aggregator = Aggregator::new(adapter.echoes_prompt());
    let mut events = aggregator.begin(prompt);
    for line in content.split(|byte| *byte == b'\n') {
        events.extend(translate_line(line, adapter, &mut aggregator).events);
    }
    let (rest, _) = conclude(aggregator, adapter, wrapped, &exit, &stderr, started);
    events.extend(rest);
    events
        .iter()
        .map(|event| without_timing(serde_json::from_str(&event.to_json()).expect("event is JSON")))
        .collect()
}

pub fn without_timing(mut event: Value) -> Value {
    if let Some(fields) = event.as_object_mut() {
        fields.remove("time");
        fields.remove("duration_ms");
    }
    event
}

pub fn events(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| without_timing(serde_json::from_str(line).unwrap()))
        .collect()
}

pub fn texts(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event["type"] == "text")
        .map(|event| event["text"].as_str().unwrap().to_string())
        .collect()
}

pub fn text_of(events: &[Value], index: usize) -> String {
    events
        .iter()
        .filter(|event| event["type"] == "text")
        .nth(index)
        .map(|event| event["text"].as_str().unwrap().to_string())
        .unwrap_or_else(|| panic!("no text event {index} in {events:?}"))
}

pub fn network_events(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event["type"] == "network")
        .cloned()
        .collect()
}

pub fn open_pty(rows: u16, columns: u16) -> (File, File) {
    let mut master = 0;
    let mut slave = 0;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let _guard = process::spawn_lock();
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    assert_eq!(opened, 0, "openpty");
    for fd in [master, slave] {
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            0,
            "FD_CLOEXEC"
        );
    }
    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

#[cfg(target_os = "linux")]
pub fn system_bwrap() -> Option<PathBuf> {
    ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

pub fn fixture(runtime: &str, name: &str, suffix: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(runtime)
        .join(format!("{name}.{suffix}"))
}

pub fn assert_replay(adapter: &mut dyn Adapter, runtime: &str, name: &str, prompt: &str) {
    let actual = replay(adapter, &fixture(runtime, name, "raw.jsonl"), prompt);
    let expected: Vec<Value> = std::fs::read_to_string(fixture(runtime, name, "events.jsonl"))
        .expect("events fixture is readable")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| without_timing(serde_json::from_str(line).expect("expected event is JSON")))
        .collect();
    assert_eq!(actual, expected);
}

pub struct WebServer {
    pub port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<usize>>,
}

impl WebServer {
    pub fn start() -> WebServer {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut served = 0;
            while !flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                            .unwrap();
                        let mut head = Vec::new();
                        let mut byte = [0u8; 1];
                        while !head.ends_with(b"\r\n\r\n")
                            && stream.read(&mut byte).is_ok_and(|n| n == 1)
                        {
                            head.push(byte[0]);
                        }
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                        served += 1;
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
            served
        });
        WebServer {
            port,
            stop,
            thread: Some(thread),
        }
    }

    pub fn served(mut self) -> usize {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap()
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(target_os = "linux")]
pub fn sandbox_available() -> bool {
    let _guard = process::spawn_lock();
    let available = std::process::Command::new("bwrap")
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
        .status()
        .is_ok_and(|status| status.success());
    if !available {
        eprintln!("skipped: bwrap is not available here");
    }
    available
}

#[cfg(target_os = "macos")]
pub fn sandbox_available() -> bool {
    true
}

#[cfg(target_os = "linux")]
pub const SANDBOX_KIND: &str = "bubblewrap";

#[cfg(target_os = "macos")]
pub const SANDBOX_KIND: &str = "seatbelt";
