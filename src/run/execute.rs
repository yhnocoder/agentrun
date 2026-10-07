use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

use super::signal::Signals;
use super::{Caller, Guards, Resources, create_raw, elapsed_ms};
use crate::cli::Runtime;
use crate::network::{FilterProxy, Policy};
use crate::output::{
    Aggregator, Body, End, EndStatus, Event, Network, NetworkInfo, OpenTools, Output,
    REFRESH_PERIOD, SandboxKind, Signal, Start, Translated,
};
use crate::process_tree;
use crate::runtime::{
    Adapter, DETAIL_MAX_CHARS, Failure, Invocation, Launch, PrivateDir, detail_tail,
};
use crate::sandbox::wrapper_failure;

const STDERR_TAIL_BYTES: usize = DETAIL_MAX_CHARS * 4 + 3;
const TEMP_ENV_VARS: [&str; 3] = ["TMPDIR", "TMP", "TEMP"];
const DRAIN_PERIOD: Duration = Duration::from_secs(1);

pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<Signal>,
    pub timed_out: bool,
}

pub(super) fn execute(
    caller: &mut Caller,
    resources: Resources,
    guards: &mut Guards,
    started: Instant,
) -> Result<u8, String> {
    let Resources {
        invocation,
        adapter,
        launch,
        plan,
        raw,
    } = resources;
    let debug = invocation.args.debug;
    let (sender, receiver) = mpsc::channel();
    if let Some(proxy) = guards.proxy.as_mut() {
        serve_proxy(caller, proxy, &invocation, &launch, sender.clone());
    }
    let raw = open_raw(raw, &invocation.tempdir, debug)?;
    if debug {
        print_debug_plan(
            caller,
            &plan,
            &invocation.tempdir,
            &launch.private_dirs,
            raw.as_ref().map(|(path, _)| path.as_path()),
        );
    }
    let color = caller.var("NO_COLOR").is_none_or(|value| value.is_empty());
    let mut output = Output::select(
        invocation.format,
        caller.stdout_is_terminal,
        color,
        invocation.sandbox.mode,
        &invocation.sandbox.reason,
    );
    let rich_stderr = output.is_rich() && caller.stderr_is_terminal;

    let hold = caller.signals.hold();
    let child = match spawn_runtime(&invocation, &launch, &invocation.tempdir) {
        Ok(child) => child,
        Err(detail) => {
            caller.signals.finishing();
            let end = Event::now(Body::End(End::early(EndStatus::Failed, detail, started)));
            caller.emit(&mut output, &end);
            if let Some(tempdir) = guards.tempdir.take() {
                finish_tempdir(tempdir, debug);
            }
            return Ok(EndStatus::Failed.exit_code());
        }
    };
    caller.signals.running(
        child.id() as i32,
        invocation.args.timeout.map(Duration::from_secs),
        launch.signal_wrapped_child,
    );
    drop(hold);

    let refresh = output.is_rich();
    let mut relay = Relay::new(
        caller,
        output,
        adapter,
        raw.map(|(_, file)| file),
        debug,
        rich_stderr,
    );
    relay.begin(start_event(&invocation, &launch), &invocation.prompt);
    let wrapped = launch.wrapped;
    let tail = spawn_pumps(child, launch.stdin, caller, sender, refresh, rich_stderr);

    let exit_code = supervise(&receiver, &mut relay);
    drop(receiver);
    guards.proxy = None;

    let tail = tail.lock().map(|tail| tail.clone()).unwrap_or_default();
    let stderr_tail = stderr_tail(&String::from_utf8_lossy(&tail));
    let exit = Exit {
        code: exit_code,
        signal: caller.signals.first_signal(),
        timed_out: caller.signals.timed_out(),
    };
    let status = relay.finish(&exit, wrapped, &stderr_tail, started);
    if let Some(tempdir) = guards.tempdir.take() {
        finish_tempdir(tempdir, debug);
    }
    Ok(status.exit_code())
}

fn serve_proxy(
    caller: &Caller,
    proxy: &mut FilterProxy,
    invocation: &Invocation,
    launch: &Launch,
    sender: Sender<Message>,
) {
    let policy = Policy {
        mode: invocation.args.network,
        rules: invocation.allow_hosts.clone(),
        service_hosts: launch.service_hosts.clone(),
    };
    let notes = proxy.serve_session(policy, &invocation.session.env, move |network| {
        let _ = sender.send(Message::Network(network));
    });
    if invocation.args.debug {
        for note in notes {
            caller.print_error_line(&format!("[debug] {note}"));
        }
    }
}

fn open_raw(
    raw: Option<(PathBuf, File)>,
    tempdir: &Path,
    debug: bool,
) -> Result<Option<(PathBuf, File)>, String> {
    match raw {
        Some(raw) => Ok(Some(raw)),
        None if debug => {
            let path = tempdir.join("raw.jsonl");
            let file = create_raw(&path)?;
            Ok(Some((path, file)))
        }
        None => Ok(None),
    }
}

fn print_debug_plan(
    caller: &Caller,
    plan: &[String],
    tempdir: &Path,
    private_dirs: &[PrivateDir],
    raw: Option<&Path>,
) {
    for line in plan {
        caller.print_error_line(&format!("[debug] {line}"));
    }
    caller.print_error_line(&format!("[debug] tempdir: {}", tempdir.display()));
    for dir in private_dirs {
        caller.print_error_line(&format!("[debug] {}: {}", dir.label, dir.path.display()));
    }
    if let Some(path) = raw {
        caller.print_error_line(&format!("[debug] raw: {}", path.display()));
    }
}

fn spawn_runtime(
    invocation: &Invocation,
    launch: &Launch,
    tempdir: &Path,
) -> Result<Child, String> {
    let (program, program_args) = launch
        .argv
        .split_first()
        .expect("adapter argv starts with the executable");
    let mut command = Command::new(program);
    command
        .args(program_args)
        .current_dir(&invocation.cwd)
        .env_clear()
        .envs(&invocation.session.env)
        .envs(launch.env.iter().map(|(key, value)| (key, value)))
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in TEMP_ENV_VARS {
        command.env(key, tempdir);
    }
    command
        .spawn()
        .map_err(|error| format!("failed to start {program}: {error}"))
}

fn start_event(invocation: &Invocation, launch: &Launch) -> Event {
    Event::now(Body::Start(Start {
        runtime: invocation.runtime,
        sandbox: invocation.sandbox.kind(),
        network: NetworkInfo {
            mode: invocation.args.network,
            allow: invocation.args.allow_host.clone(),
            enforced: invocation.sandbox.runs(),
        },
        model: invocation.args.model.clone(),
        cwd: invocation.cwd.to_string_lossy().into_owned(),
        argv: launch
            .argv
            .iter()
            .filter(|arg| **arg != invocation.prompt)
            .cloned()
            .collect(),
        env: invocation.session.set.clone(),
    }))
}

fn spawn_pumps(
    mut child: Child,
    stdin: Vec<u8>,
    caller: &Caller,
    sender: Sender<Message>,
    refresh: bool,
    relay_stderr: bool,
) -> Arc<Mutex<Vec<u8>>> {
    let mut child_stdin = child.stdin.take().expect("stdin is piped");
    thread::spawn(move || {
        let _ = child_stdin.write_all(&stdin);
    });
    let stderr = child.stderr.take().expect("stderr is piped");
    let sink = Arc::clone(&caller.stderr);
    let tail = Arc::new(Mutex::new(Vec::new()));
    let tail_writer = Arc::clone(&tail);
    let stderr_sender = sender.clone();
    let relay = relay_stderr.then(|| sender.clone());
    thread::spawn(move || {
        forward_stderr(stderr, sink, relay, &tail_writer);
        let _ = stderr_sender.send(Message::StderrEnd);
    });
    let stdout = child.stdout.take().expect("stdout is piped");
    let stdout_sender = sender.clone();
    thread::spawn(move || read_lines(stdout, &stdout_sender));
    if refresh {
        let refresh_sender = sender.clone();
        thread::spawn(move || {
            while refresh_sender.send(Message::Refresh).is_ok() {
                thread::sleep(REFRESH_PERIOD);
            }
        });
    }
    let exit_signals = caller.signals.clone();
    thread::spawn(move || wait_child(child, &exit_signals, &sender));
    tail
}

struct Relay<'a> {
    caller: &'a Caller,
    output: Output,
    aggregator: Aggregator,
    adapter: Box<dyn Adapter>,
    raw: Option<File>,
    debug: bool,
    rich_stderr: bool,
}

impl<'a> Relay<'a> {
    fn new(
        caller: &'a Caller,
        output: Output,
        adapter: Box<dyn Adapter>,
        raw: Option<File>,
        debug: bool,
        rich_stderr: bool,
    ) -> Relay<'a> {
        Relay {
            caller,
            output,
            aggregator: Aggregator::new(adapter.echoes_prompt()),
            adapter,
            raw,
            debug,
            rich_stderr,
        }
    }

    fn begin(&mut self, start: Event, prompt: &str) {
        self.caller.emit(&mut self.output, &start);
        for event in self.aggregator.begin(prompt) {
            self.caller.emit(&mut self.output, &event);
        }
    }

    fn line(&mut self, content: &[u8]) {
        if let Some(file) = self.raw.as_mut() {
            let _ = file.write_all(content);
            let _ = file.write_all(b"\n");
        }
        let translated = translate_line(content, self.adapter.as_mut(), &mut self.aggregator);
        if self.output.is_rich() {
            self.output.set_open_tools(self.aggregator.open_tools());
        }
        for event in &translated.events {
            self.caller.emit(&mut self.output, event);
        }
        if self.debug {
            for line in &translated.debug {
                let line = format!("[debug] {line}\n");
                if self.rich_stderr {
                    self.caller
                        .with_stdout(|stdout| self.output.stderr(stdout, line.as_bytes()));
                } else {
                    self.caller.print_error_line(line.trim_end());
                }
            }
        }
        if translated.terminate {
            self.caller.signals.kill_group();
        }
    }

    fn stderr(&mut self, bytes: &[u8]) {
        self.caller
            .with_stdout(|stdout| self.output.stderr(stdout, bytes));
    }

    fn network(&mut self, network: Network) {
        self.caller
            .emit(&mut self.output, &Event::now(Body::Network(network)));
    }

    fn refresh(&mut self) {
        self.caller
            .with_stdout(|stdout| self.output.refresh(stdout));
    }

    fn finish(
        mut self,
        exit: &Exit,
        wrapped: SandboxKind,
        stderr_tail: &str,
        started: Instant,
    ) -> EndStatus {
        if self.output.is_rich() {
            self.output.set_open_tools(OpenTools::default());
        }
        let (events, status) = conclude(
            self.aggregator,
            self.adapter.as_mut(),
            wrapped,
            exit,
            stderr_tail,
            started,
        );
        for event in &events {
            self.caller.emit(&mut self.output, event);
        }
        drop(self.raw);
        status
    }
}

enum Message {
    Line(Vec<u8>),
    Stderr(Vec<u8>),
    Refresh,
    Network(Network),
    StdoutEnd,
    StderrEnd,
    Exited(ExitStatus),
}

fn supervise(receiver: &Receiver<Message>, relay: &mut Relay) -> Option<i32> {
    let mut exit_code = None;
    let mut exited = false;
    let mut stdout_ended = false;
    let mut stderr_ended = false;
    let mut drain_deadline: Option<Instant> = None;
    loop {
        let message = match drain_deadline {
            None => receiver.recv().ok(),
            Some(deadline) => {
                match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(message) => Some(message),
                    Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => None,
                }
            }
        };
        match message {
            Some(Message::Line(line)) => {
                let content = line.strip_suffix(b"\n").unwrap_or(&line);
                relay.line(content);
            }
            Some(Message::Stderr(bytes)) => relay.stderr(&bytes),
            Some(Message::Refresh) => relay.refresh(),
            Some(Message::Network(network)) => relay.network(network),
            Some(Message::StdoutEnd) => stdout_ended = true,
            Some(Message::StderrEnd) => stderr_ended = true,
            Some(Message::Exited(status)) => {
                exited = true;
                exit_code = status.code();
                drain_deadline = Some(Instant::now() + DRAIN_PERIOD);
            }
            None => return exit_code,
        }
        if exited && stdout_ended && stderr_ended {
            return exit_code;
        }
    }
}

fn read_lines(stdout: impl Read, sender: &Sender<Message>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        if sender
            .send(Message::Line(std::mem::take(&mut line)))
            .is_err()
        {
            return;
        }
    }
    let _ = sender.send(Message::StdoutEnd);
}

fn wait_child(mut child: Child, signals: &Signals, sender: &Sender<Message>) {
    process_tree::wait_for_exit(child.id() as i32, true);
    signals.exited();
    if let Ok(status) = child.wait() {
        let _ = sender.send(Message::Exited(status));
    }
}

pub fn translate_line(
    line: &[u8],
    adapter: &mut dyn Adapter,
    aggregator: &mut Aggregator,
) -> Translated {
    let records = match serde_json::from_slice::<Value>(line) {
        Ok(value) => adapter.translate(&value),
        Err(_) => Vec::new(),
    };
    aggregator.push(records)
}

fn forward_stderr(
    mut stderr: impl Read,
    sink: Arc<Mutex<dyn Write + Send>>,
    relay: Option<Sender<Message>>,
    tail: &Mutex<Vec<u8>>,
) {
    let mut buffer = [0u8; 8192];
    loop {
        let count = match stderr.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        match &relay {
            Some(relay) => {
                let _ = relay.send(Message::Stderr(buffer[..count].to_vec()));
            }
            None => {
                if let Ok(mut sink) = sink.lock() {
                    let _ = sink.write_all(&buffer[..count]);
                    let _ = sink.flush();
                }
            }
        }
        if let Ok(mut tail) = tail.lock() {
            tail.extend_from_slice(&buffer[..count]);
            if tail.len() > STDERR_TAIL_BYTES {
                let excess = tail.len() - STDERR_TAIL_BYTES;
                tail.drain(..excess);
            }
        }
    }
}

pub fn stderr_tail(stderr: &str) -> String {
    detail_tail(stderr.trim_end())
}

fn finish_tempdir(tempdir: TempDir, debug: bool) {
    if debug {
        let _ = tempdir.keep();
    }
}

pub fn conclude(
    mut aggregator: Aggregator,
    adapter: &mut dyn Adapter,
    wrapped: SandboxKind,
    exit: &Exit,
    stderr_tail: &str,
    started: Instant,
) -> (Vec<Event>, EndStatus) {
    let mut events = aggregator.push(adapter.after_exit()).events;
    let (rest, summary) = aggregator.finish();
    events.extend(rest);
    let (status, detail) = match (exit.signal, exit.timed_out) {
        (Some(signal), _) => (EndStatus::Interrupted(signal), String::new()),
        (None, true) => (EndStatus::Timeout, String::new()),
        (None, false) => match adapter.failure(exit.code) {
            None => (EndStatus::Finished, String::new()),
            Some(Failure::Message(detail)) => (EndStatus::Failed, detail),
            Some(Failure::Unexplained) => (
                EndStatus::Failed,
                wrapper_failure(wrapped, exit.code, stderr_tail)
                    .unwrap_or_else(|| unexplained_detail(stderr_tail, adapter.runtime())),
            ),
        },
    };
    events.push(Event::now(Body::End(End {
        status,
        exit_code: exit.code,
        detail,
        duration_ms: elapsed_ms(started),
        usage: summary.usage,
        result: summary.result,
    })));
    (events, status)
}

fn unexplained_detail(stderr_tail: &str, runtime: Runtime) -> String {
    let stderr_tail = stderr_tail.trim();
    if !stderr_tail.is_empty() {
        return stderr_tail.to_string();
    }
    format!("{} failed without an error message", runtime.name())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::cli::RunArgs;
    use crate::output::Record;

    struct Reported(Option<Failure>);

    impl Adapter for Reported {
        fn runtime(&self) -> Runtime {
            Runtime::Pi
        }

        fn check_args(&self, _: &RunArgs) -> Result<(), String> {
            Ok(())
        }

        fn launch(&mut self, _: &Path, _: &Invocation) -> Result<Launch, String> {
            unreachable!("conclude does not launch")
        }

        fn echoes_prompt(&self) -> bool {
            false
        }

        fn translate(&mut self, _: &Value) -> Vec<Record> {
            Vec::new()
        }

        fn after_exit(&mut self) -> Vec<Record> {
            Vec::new()
        }

        fn failure(&self, _: Option<i32>) -> Option<Failure> {
            self.0.clone()
        }
    }

    fn detail(failure: Option<Failure>, wrapped: SandboxKind, code: i32, stderr: &str) -> String {
        let exit = Exit {
            code: Some(code),
            signal: None,
            timed_out: false,
        };
        let (events, _) = conclude(
            Aggregator::new(false),
            &mut Reported(failure),
            wrapped,
            &exit,
            &stderr_tail(stderr),
            Instant::now(),
        );
        match &events.last().unwrap().body {
            Body::End(end) => end.detail.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unexplained_failure_checks_the_wrapper_before_the_stderr_tail() {
        let bwrap = "starting\nbwrap: Can't mkdir /x: Permission denied\n";
        let unexplained = || Some(Failure::Unexplained);
        assert_eq!(
            detail(unexplained(), SandboxKind::Bubblewrap, 1, bwrap),
            "sandbox failed to start: Can't mkdir /x: Permission denied"
        );
        assert_eq!(
            detail(
                unexplained(),
                SandboxKind::Seatbelt,
                65,
                "sandbox-exec: syntax error\n"
            ),
            "sandbox failed to start: syntax error"
        );
        assert_eq!(
            detail(unexplained(), SandboxKind::Seatbelt, 1, "bwrap: x\n"),
            "bwrap: x"
        );
        assert_eq!(
            detail(unexplained(), SandboxKind::None, 1, bwrap),
            "starting\nbwrap: Can't mkdir /x: Permission denied"
        );
        assert_eq!(
            detail(unexplained(), SandboxKind::Bubblewrap, 0, bwrap),
            "starting\nbwrap: Can't mkdir /x: Permission denied"
        );
        assert_eq!(
            detail(unexplained(), SandboxKind::Bubblewrap, 1, ""),
            "pi failed without an error message"
        );
    }

    #[test]
    fn failure_message_comes_before_the_wrapper() {
        assert_eq!(
            detail(
                Some(Failure::Message("pi uses openai/gpt".to_string())),
                SandboxKind::Bubblewrap,
                1,
                "bwrap: Can't mkdir /x: Permission denied\n"
            ),
            "pi uses openai/gpt"
        );
        assert_eq!(detail(None, SandboxKind::Bubblewrap, 0, ""), "");
    }

    #[test]
    fn tail_keeps_last_characters() {
        let stderr = format!("{}修复 \n\n", "a".repeat(600));
        assert_eq!(stderr_tail(&stderr), format!("{}修复", "a".repeat(498)));
        assert_eq!(stderr_tail("short\n"), "short");
    }
}
