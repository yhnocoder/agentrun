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
use super::{Caller, Guards, Resources, create_raw, elapsed_ms, path_texts};
use crate::cli::Runtime;
use crate::network::{FilterProxy, Policy};
use crate::output::{
    Aggregator, Body, End, EndStatus, Event, Network, NetworkInfo, Output, REFRESH_PERIOD,
    SandboxKind, Signal, Start, Translated,
};
use crate::process_tree;
use crate::runtime::{
    Adapter, DETAIL_MAX_CHARS, Failure, Invocation, Launch, PrivateDir, contains, detail_tail,
};
use crate::sandbox::wrapper_failure;
use crate::transcript;

const STDERR_TAIL_BYTES: usize = DETAIL_MAX_CHARS * 4 + 3;
const TEMP_ENV_VARS: [&str; 3] = ["TMPDIR", "TMP", "TEMP"];
const DRAIN_PERIOD: Duration = Duration::from_secs(1);

pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<Signal>,
    pub timeout: Option<String>,
    pub limit: Option<String>,
}

#[derive(Clone, Copy)]
struct Limits {
    max_turns: Option<u64>,
    max_tokens: Option<u64>,
}

impl Limits {
    fn exceeded(&self, aggregator: &Aggregator) -> Option<String> {
        let calls = aggregator.main_calls();
        if let Some(max) = self.max_turns.filter(|max| calls > *max) {
            return Some(format!(
                "--max-turns {max} reached: the main agent started turn {calls}"
            ));
        }
        let tokens = aggregator.tokens();
        if let Some(max) = self.max_tokens.filter(|max| tokens > *max) {
            return Some(format!("--max-tokens {max} exceeded: {tokens} tokens"));
        }
        None
    }
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
    if let Some(note) = session_dir_note(&invocation) {
        output.add_note(note);
    }
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
    let limits = Limits {
        max_turns: invocation.args.max_turns,
        max_tokens: invocation.args.max_tokens,
    };
    let mut relay = Relay::new(
        caller,
        output,
        adapter,
        raw.map(|(_, file)| file),
        debug,
        rich_stderr,
        limits,
    );
    relay.begin(
        start_event(&invocation, &launch),
        &invocation.prompt,
        invocation
            .images
            .iter()
            .map(|image| image.given.clone())
            .collect(),
    );
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
        timeout: invocation
            .args
            .timeout
            .filter(|_| caller.signals.timed_out())
            .map(|seconds| format!("--timeout {seconds} reached")),
        limit: relay.limit.take(),
    };
    let store = invocation
        .args
        .session_dir
        .as_deref()
        .map(|dir| transcript::store(dir, invocation.runtime));
    let status = relay.finish(&exit, wrapped, &stderr_tail, store.as_deref(), started);
    if let Some(tempdir) = guards.tempdir.take() {
        finish_tempdir(tempdir, debug);
    }
    Ok(status.exit_code())
}

fn session_dir_note(invocation: &Invocation) -> Option<String> {
    let dir = invocation.args.session_dir.as_deref()?;
    (invocation.sandbox.runs() && contains(&invocation.cwd, dir)).then(|| {
        format!(
            "--session-dir {} is inside the working directory. The agent can change the saved sessions",
            dir.display()
        )
    })
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
        deny_read: path_texts(&invocation.denied_reads),
        model: invocation.args.model.clone(),
        cwd: invocation.cwd.to_string_lossy().into_owned(),
        argv: launch.argv.clone(),
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
    limits: Limits,
    limit: Option<String>,
}

impl<'a> Relay<'a> {
    fn new(
        caller: &'a Caller,
        output: Output,
        adapter: Box<dyn Adapter>,
        raw: Option<File>,
        debug: bool,
        rich_stderr: bool,
        limits: Limits,
    ) -> Relay<'a> {
        Relay {
            caller,
            output,
            aggregator: Aggregator::new(adapter.echoes_prompt()),
            adapter,
            raw,
            debug,
            rich_stderr,
            limits,
            limit: None,
        }
    }

    fn begin(&mut self, start: Event, prompt: &str, images: Vec<String>) {
        self.caller.emit(&mut self.output, &start);
        for event in self.aggregator.begin(prompt, images) {
            self.caller.emit(&mut self.output, &event);
        }
    }

    fn save_transcript(&mut self, store: &Path) -> Option<String> {
        let saved = self.adapter.transcript()?;
        match transcript::save(&saved, store) {
            Ok(()) => Some(saved.id),
            Err(detail) => {
                self.error_line(&format!("agentrun: cannot save the session: {detail}"));
                None
            }
        }
    }

    fn error_line(&mut self, line: &str) {
        if self.rich_stderr {
            let line = format!("{line}\n");
            self.caller
                .with_stdout(|stdout| self.output.stderr(stdout, line.as_bytes()));
        } else {
            self.caller.print_error_line(line);
        }
    }

    fn line(&mut self, content: &[u8]) {
        if let Some(file) = self.raw.as_mut() {
            let _ = file.write_all(content);
            let _ = file.write_all(b"\n");
        }
        let translated = translate_line(content, self.adapter.as_mut(), &mut self.aggregator);
        for event in &translated.events {
            self.caller.emit(&mut self.output, event);
        }
        if self.debug {
            for line in &translated.debug {
                self.error_line(&format!("[debug] {line}"));
            }
        }
        if translated.terminate {
            self.caller.signals.kill_group();
        } else if self.limit.is_none()
            && let Some(detail) = self.limits.exceeded(&self.aggregator)
            && self.caller.signals.stop_for_limit()
        {
            self.limit = Some(detail);
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
        store: Option<&Path>,
        started: Instant,
    ) -> EndStatus {
        let session_id = store.and_then(|store| self.save_transcript(store));
        let (events, status) = conclude(
            self.aggregator,
            self.adapter.as_mut(),
            wrapped,
            exit,
            stderr_tail,
            session_id,
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
    session_id: Option<String>,
    started: Instant,
) -> (Vec<Event>, EndStatus) {
    let mut events = aggregator.push(adapter.after_exit()).events;
    let (rest, summary) = aggregator.finish();
    events.extend(rest);
    let (status, detail) = match (exit.signal, &exit.timeout, &exit.limit) {
        (Some(signal), _, _) => (EndStatus::Interrupted(signal), String::new()),
        (None, Some(timeout), _) => (EndStatus::Timeout, timeout.clone()),
        (None, None, limit) => match (adapter.failure(exit.code), limit) {
            // why(#86): 不能改成按是否发过 SIGTERM 判定，运行时已经在退出、退出码定为 0 之后才收到信号时会被误记为 limit
            (None, _) => (EndStatus::Finished, String::new()),
            (Some(_), Some(limit)) => (EndStatus::Limit, limit.clone()),
            (Some(Failure::Message(detail)), None) => (EndStatus::Failed, detail),
            (Some(Failure::Unexplained), None) => (
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
        session_id,
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
    use crate::output::{Record, Signal};
    use crate::transcript::Transcript;

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

        fn transcript(&self) -> Option<Transcript> {
            None
        }
    }

    fn detail(failure: Option<Failure>, wrapped: SandboxKind, code: i32, stderr: &str) -> String {
        let exit = Exit {
            code: Some(code),
            signal: None,
            timeout: None,
            limit: None,
        };
        let (events, _) = conclude(
            Aggregator::new(false),
            &mut Reported(failure),
            wrapped,
            &exit,
            &stderr_tail(stderr),
            None,
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

    fn status(exit: Exit, failure: Option<Failure>) -> (EndStatus, String) {
        let (events, status) = conclude(
            Aggregator::new(false),
            &mut Reported(failure),
            SandboxKind::None,
            &exit,
            "",
            None,
            Instant::now(),
        );
        match &events.last().unwrap().body {
            Body::End(end) => (status, end.detail.clone()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn limit_comes_after_signals_and_timeout_and_only_replaces_a_failure() {
        let exit = |signal, timeout: Option<&str>, limit: Option<&str>| Exit {
            code: None,
            signal,
            timeout: timeout.map(str::to_string),
            limit: limit.map(str::to_string),
        };
        let failed = || Some(Failure::Message("failed".to_string()));
        let timeout = Some("--timeout 5 reached");
        assert_eq!(
            status(
                exit(Some(Signal::Interrupt), timeout, Some("over")),
                failed()
            ),
            (EndStatus::Interrupted(Signal::Interrupt), String::new())
        );
        assert_eq!(
            status(exit(None, timeout, Some("over")), failed()),
            (EndStatus::Timeout, "--timeout 5 reached".to_string())
        );
        assert_eq!(
            status(exit(None, None, Some("over")), failed()),
            (EndStatus::Limit, "over".to_string())
        );
        assert_eq!(
            status(exit(None, None, None), failed()),
            (EndStatus::Failed, "failed".to_string())
        );
        assert_eq!(
            status(exit(None, None, Some("over")), None),
            (EndStatus::Finished, String::new())
        );
    }

    fn aggregated(records: Vec<Record>) -> Aggregator {
        let mut aggregator = Aggregator::new(false);
        aggregator.push(records);
        aggregator
    }

    #[test]
    fn limits_name_the_option_the_limit_and_the_count() {
        let call = |parent: Option<&str>| Record::CallStart {
            parent: parent.map(str::to_string),
        };
        let usage = |output| Record::Usage {
            parent: Some("s1".to_string()),
            model: None,
            counts: crate::output::TokenCounts {
                input_tokens: Some(4),
                output_tokens: Some(output),
                cache_read_tokens: None,
                cache_write_tokens: Some(1),
                cost_usd: None,
            },
        };
        let subagent = Record::SubagentStart {
            id: "s1".to_string(),
            parent: None,
            kind: "Explore".to_string(),
            model: None,
            description: String::new(),
        };
        let limits = |max_turns, max_tokens| Limits {
            max_turns,
            max_tokens,
        };
        let two_calls = aggregated(vec![
            subagent.clone(),
            call(None),
            call(Some("s1")),
            call(Some("unknown")),
        ]);
        assert_eq!(limits(Some(2), None).exceeded(&two_calls), None);
        assert_eq!(
            limits(Some(1), Some(1)).exceeded(&two_calls),
            Some("--max-turns 1 reached: the main agent started turn 2".to_string())
        );
        let tokens = aggregated(vec![subagent, usage(5), usage(0)]);
        assert_eq!(limits(None, Some(15)).exceeded(&tokens), None);
        assert_eq!(
            limits(Some(1), Some(14)).exceeded(&tokens),
            Some("--max-tokens 14 exceeded: 15 tokens".to_string())
        );
    }

    #[test]
    fn tail_keeps_last_characters() {
        let stderr = format!("{}修复 \n\n", "a".repeat(600));
        assert_eq!(stderr_tail(&stderr), format!("{}修复", "a".repeat(498)));
        assert_eq!(stderr_tail("short\n"), "short");
    }
}
