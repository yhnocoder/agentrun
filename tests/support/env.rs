use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use agentrun::adapter::Adapter;
use agentrun::run::{Caller, run};
use agentrun::signal::Signals;
use serde_json::Value;
use tempfile::TempDir;

use super::fake::FakeAdapter;
use super::process::{FAKE_AGENTRUN, spawn_lock};

pub const WAIT_LIMIT: Duration = Duration::from_secs(30);
pub const POLL: Duration = Duration::from_millis(20);

pub fn poll_until(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(POLL);
    }
    condition()
}

pub fn wait_until(what: &str, condition: impl FnMut() -> bool) {
    assert!(
        poll_until(WAIT_LIMIT, condition),
        "timed out waiting for {what}"
    );
}

pub fn write_script(path: &Path, content: &str) {
    let _guard = spawn_lock();
    let mut writer = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    writer
        .stdin
        .take()
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
    let status = writer.wait().unwrap();
    assert!(
        status.success(),
        "cannot write the script {}: {status}",
        path.display()
    );
}

pub struct Env {
    root: TempDir,
}

impl Env {
    pub fn new() -> Env {
        let env = Env {
            root: tempfile::tempdir().unwrap(),
        };
        for dir in [env.bin(), env.tmp(), env.work(), env.home()] {
            std::fs::create_dir(dir).unwrap();
        }
        env
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn bin(&self) -> PathBuf {
        self.root().join("bin")
    }

    pub fn tmp(&self) -> PathBuf {
        self.root().join("tmp")
    }

    pub fn work(&self) -> PathBuf {
        self.root().join("work")
    }

    pub fn home(&self) -> PathBuf {
        self.root().join("home")
    }

    pub fn path_var(&self) -> String {
        format!("{}:/usr/bin:/bin", self.bin().display())
    }

    pub fn install(&self, name: &str, script: &str) -> PathBuf {
        let path = self.bin().join(name);
        write_script(&path, script);
        path
    }

    pub fn leftovers(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.tmp())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    pub fn command(&self, args: &[&str]) -> Command {
        self.command_for(env!("CARGO_BIN_EXE_agentrun"), args)
    }

    pub fn fake_command(&self, args: &[&str]) -> Command {
        let mut command = self.command_for(std::env::current_exe().unwrap(), args);
        command.env(FAKE_AGENTRUN, "1");
        command
    }

    fn command_for(&self, program: impl AsRef<OsStr>, args: &[&str]) -> Command {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(self.root())
            .env_clear()
            .env("PATH", self.path_var())
            .env("TMPDIR", self.tmp())
            .env("HOME", self.home())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    pub fn run_in_process(
        &self,
        runtime: &str,
        caller_env: &[(&str, &str)],
        extra: &[&str],
        echoes: bool,
    ) -> Outcome {
        let mut args: Vec<OsString> = vec![
            "agentrun".into(),
            runtime.into(),
            "--cwd".into(),
            self.work().into(),
            "--prompt".into(),
            "hi".into(),
        ];
        args.extend(extra.iter().map(OsString::from));
        if !extra.contains(&"--sandbox") {
            args.splice(2..2, ["--sandbox".into(), "off".into()]);
        }
        let mut env = vec![
            (OsString::from("PATH"), OsString::from(self.path_var())),
            (OsString::from("TMPDIR"), self.tmp().into_os_string()),
        ];
        env.extend(
            caller_env
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        );
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let caller = Caller {
            args,
            env,
            stdin: Box::new(std::io::empty()),
            stdin_is_terminal: false,
            stdout: stdout.clone(),
            stdout_is_terminal: false,
            stderr: stderr.clone(),
            stderr_is_terminal: false,
            signals: Signals::install(),
        };
        let code = run(caller, &move |_| {
            Box::new(FakeAdapter::new(echoes)) as Box<dyn Adapter>
        });
        let text =
            |bytes: &Arc<Mutex<Vec<u8>>>| String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        Outcome {
            code,
            stdout: text(&stdout),
            stderr: text(&stderr),
        }
    }
}

pub struct Outcome {
    pub code: u8,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    pub fn events(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    pub fn stripped(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .map(super::without_timing)
            .collect()
    }

    pub fn end(&self) -> Value {
        self.events().pop().unwrap()
    }
}

type Readers = (JoinHandle<()>, JoinHandle<String>);

pub struct Agentrun {
    child: Child,
    events: Arc<Mutex<Vec<Value>>>,
    readers: Option<Readers>,
    started: Instant,
}

pub struct Finished {
    pub code: i32,
    pub events: Vec<Value>,
    pub stderr: String,
    pub elapsed: Duration,
}

impl Agentrun {
    pub fn spawn(mut command: Command) -> Agentrun {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = {
            let _guard = spawn_lock();
            command.spawn().unwrap()
        };
        let started = Instant::now();
        let events = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&events);
        let stdout = child.stdout.take().unwrap();
        let stdout_reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                let event = serde_json::from_str(&line)
                    .map(super::without_timing)
                    .unwrap_or_else(|_| Value::String(line.clone()));
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
            started,
        }
    }

    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    pub fn signal(&self, signal: i32) {
        unsafe {
            libc::kill(self.pid(), signal);
        }
    }

    pub fn take_stdin(&mut self) -> ChildStdin {
        self.child
            .stdin
            .take()
            .expect("the agentrun command has no piped stdin")
    }

    pub fn events(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }

    pub fn wait_for_text(&self, text: &str) {
        let found = poll_until(WAIT_LIMIT, || {
            self.events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event["type"] == "text" && event["text"] == text)
        });
        assert!(
            found,
            "timed out waiting for the text {text:?}; agentrun pid {} events {:?}",
            self.pid(),
            self.events()
        );
    }

    pub fn wait(mut self) -> Finished {
        let exited = poll_until(WAIT_LIMIT, || self.child.try_wait().unwrap().is_some());
        let elapsed = self.started.elapsed();
        if !exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
            panic!(
                "agentrun {} did not finish within {WAIT_LIMIT:?}; events {:?}",
                self.pid(),
                self.events()
            );
        }
        let status = self.child.wait().unwrap();
        let (stdout_reader, stderr_reader) = self.readers.take().unwrap();
        stdout_reader.join().unwrap();
        let stderr = stderr_reader.join().unwrap();
        Finished {
            code: status
                .code()
                .unwrap_or_else(|| panic!("agentrun ended without an exit code: {status}")),
            events: self.events(),
            stderr,
            elapsed,
        }
    }
}

impl Finished {
    pub fn end(&self) -> &Value {
        let last = self.events.last();
        assert_eq!(
            last.map(|event| &event["type"]),
            Some(&Value::from("end")),
            "{:?}\n{}",
            self.events,
            self.stderr
        );
        last.unwrap()
    }

    pub fn texts(&self) -> Vec<String> {
        super::texts(&self.events)
    }
}
