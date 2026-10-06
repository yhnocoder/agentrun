use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

use super::signal::Signals;
use super::{Caller, Ready, create_raw, elapsed_ms, reject};
use crate::cli::Runtime;
use crate::network::{FilterProxy, Policy};
use crate::output::{
    Aggregator, Body, End, EndStatus, Event, Network, NetworkInfo, OpenTools, Output,
    REFRESH_PERIOD, Signal, Start, Translated,
};
use crate::process_tree;
use crate::runtime::{Adapter, DETAIL_MAX_CHARS, Launch, detail_tail};

const STDERR_TAIL_BYTES: usize = DETAIL_MAX_CHARS * 4 + 3;
const TEMP_ENV_VARS: [&str; 3] = ["TMPDIR", "TMP", "TEMP"];
const DRAIN_PERIOD: Duration = Duration::from_secs(1);

pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<Signal>,
    pub timed_out: bool,
}

pub(super) fn execute(
    mut caller: Caller,
    ready: Ready,
    launch: Launch,
    plan: Vec<String>,
    tempdir: TempDir,
    mut proxy: Option<FilterProxy>,
    started: Instant,
) -> u8 {
    let Ready {
        invocation,
        mut adapter,
        raw,
        ..
    } = ready;
    let debug = invocation.args.debug;
    let (sender, receiver) = mpsc::channel();
    if let Some(proxy) = proxy.as_mut() {
        let policy = Policy {
            mode: invocation.args.network,
            rules: invocation.allow_hosts.clone(),
            service_hosts: launch.service_hosts.clone(),
        };
        let reporter = sender.clone();
        let notes = proxy.serve_session(policy, &invocation.session.env, move |network| {
            let _ = reporter.send(Message::Network(network));
        });
        if debug {
            for note in notes {
                caller.print_error_line(&format!("[debug] {note}"));
            }
        }
    }
    let raw = match raw {
        Some(raw) => Some(raw),
        None if debug => {
            let path = tempdir.path().join("raw.jsonl");
            match create_raw(&path) {
                Ok(file) => Some((path, file)),
                Err(detail) => return reject(&mut caller, invocation.format, &detail, started),
            }
        }
        None => None,
    };
    if debug {
        for line in &plan {
            caller.print_error_line(&format!("[debug] {line}"));
        }
        caller.print_error_line(&format!("[debug] tempdir: {}", tempdir.path().display()));
        if let Some(home) = &invocation.codex_home {
            caller.print_error_line(&format!("[debug] codex home: {}", home.display()));
        }
        if let Some((path, _)) = &raw {
            caller.print_error_line(&format!("[debug] raw: {}", path.display()));
        }
    }
    let mut raw = raw.map(|(_, file)| file);
    let color = caller.var("NO_COLOR").is_none_or(|value| value.is_empty());
    let mut output = Output::select(
        invocation.format,
        caller.stdout_is_terminal,
        color,
        invocation.sandbox.mode,
        &invocation.sandbox.reason,
    );
    let rich_stderr = output.is_rich() && caller.stderr_is_terminal;

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
        command.env(key, tempdir.path());
    }
    let hold = caller.signals.hold();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            caller.signals.finishing();
            let end = Event::now(Body::End(End::early(
                EndStatus::Failed,
                format!("failed to start {program}: {error}"),
                started,
            )));
            caller.emit(&mut output, &end);
            finish_tempdir(tempdir, debug);
            return EndStatus::Failed.exit_code();
        }
    };
    caller.signals.running(
        child.id() as i32,
        invocation.args.timeout.map(Duration::from_secs),
        launch.signal_wrapped_child,
    );
    drop(hold);

    let start = Event::now(Body::Start(Start {
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
    }));
    caller.emit(&mut output, &start);
    let mut aggregator = Aggregator::new(adapter.echoes_prompt());
    for event in aggregator.begin(&invocation.prompt) {
        caller.emit(&mut output, &event);
    }

    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = launch.stdin;
    thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let stderr = child.stderr.take().expect("stderr is piped");
    let sink = Arc::clone(&caller.stderr);
    let tail = Arc::new(Mutex::new(Vec::new()));
    let tail_writer = Arc::clone(&tail);
    let stderr_sender = sender.clone();
    let relay = rich_stderr.then(|| sender.clone());
    thread::spawn(move || {
        forward_stderr(stderr, sink, relay, &tail_writer);
        let _ = stderr_sender.send(Message::StderrEnd);
    });
    let stdout = child.stdout.take().expect("stdout is piped");
    let stdout_sender = sender.clone();
    thread::spawn(move || read_lines(stdout, &stdout_sender));
    if output.is_rich() {
        let refresh_sender = sender.clone();
        thread::spawn(move || {
            while refresh_sender.send(Message::Refresh).is_ok() {
                thread::sleep(REFRESH_PERIOD);
            }
        });
    }
    let exit_signals = caller.signals.clone();
    thread::spawn(move || wait_child(child, &exit_signals, &sender));

    let exit_code = supervise(&caller, &receiver, |input| {
        let Input::Line(content) = input else {
            match input {
                Input::Network(network) => {
                    caller.emit(&mut output, &Event::now(Body::Network(network)));
                }
                Input::Stderr(bytes) => {
                    caller.with_stdout(|stdout| output.stderr(stdout, bytes));
                }
                _ => caller.with_stdout(|stdout| output.refresh(stdout)),
            }
            return false;
        };
        if let Some(file) = raw.as_mut() {
            let mut bytes = Vec::with_capacity(content.len() + 1);
            bytes.extend_from_slice(content);
            bytes.push(b'\n');
            let _ = file.write_all(&bytes);
        }
        let translated = translate_line(content, adapter.as_mut(), &mut aggregator);
        if output.is_rich() {
            output.set_open_tools(aggregator.open_tools());
        }
        for event in translated.events {
            caller.emit(&mut output, &event);
        }
        if debug {
            for line in translated.debug {
                let line = format!("[debug] {line}\n");
                if rich_stderr {
                    caller.with_stdout(|stdout| output.stderr(stdout, line.as_bytes()));
                } else {
                    caller.print_error_line(line.trim_end());
                }
            }
        }
        translated.terminate
    });
    drop(receiver);
    drop(proxy);

    let tail = tail.lock().map(|tail| tail.clone()).unwrap_or_default();
    let stderr_tail = stderr_tail(&String::from_utf8_lossy(&tail));
    let exit = Exit {
        code: exit_code,
        signal: caller.signals.first_signal(),
        timed_out: caller.signals.timed_out(),
    };
    if output.is_rich() {
        output.set_open_tools(OpenTools::default());
    }
    let (events, status) = conclude(aggregator, adapter.as_mut(), &exit, &stderr_tail, started);
    for event in &events {
        caller.emit(&mut output, event);
    }
    drop(raw);
    finish_tempdir(tempdir, debug);
    status.exit_code()
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

enum Input<'a> {
    Line(&'a [u8]),
    Stderr(&'a [u8]),
    Refresh,
    Network(Network),
}

fn supervise(
    caller: &Caller,
    receiver: &Receiver<Message>,
    mut handle: impl FnMut(Input) -> bool,
) -> Option<i32> {
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
                if handle(Input::Line(content)) {
                    caller.signals.kill_group();
                }
            }
            Some(Message::Stderr(bytes)) => {
                handle(Input::Stderr(&bytes));
            }
            Some(Message::Refresh) => {
                handle(Input::Refresh);
            }
            Some(Message::Network(network)) => {
                handle(Input::Network(network));
            }
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
        if sender.send(Message::Line(line.clone())).is_err() {
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
        (None, false) => match adapter.failure(exit.code, stderr_tail) {
            Some(detail) => (
                EndStatus::Failed,
                failure_detail(detail, stderr_tail, adapter.runtime()),
            ),
            None => (EndStatus::Finished, String::new()),
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

fn failure_detail(detail: String, stderr_tail: &str, runtime: Runtime) -> String {
    if !detail.is_empty() {
        return detail;
    }
    let stderr_tail = stderr_tail.trim();
    if !stderr_tail.is_empty() {
        return stderr_tail.to_string();
    }
    format!("{} failed without an error message", runtime.name())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_last_characters() {
        let stderr = format!("{}修复 \n\n", "a".repeat(600));
        assert_eq!(stderr_tail(&stderr), format!("{}修复", "a".repeat(498)));
        assert_eq!(stderr_tail("short\n"), "short");
    }
}
