use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use clap::error::ErrorKind as ClapErrorKind;
use serde_json::Value;
use tempfile::TempDir;

use crate::adapter::{Adapter, Invocation, Launch, Record};
use crate::cli::{
    Cli, Format, Parsed, RunArgs, Runtime, SandboxMode, default_format, prescan_format,
    usage_error_detail,
};
use crate::codex;
use crate::credential::write_session_credential;
use crate::doctor;
use crate::network::{self, FilterProxy, Policy, ProxyEndpoint, Upstream};
use crate::output::{
    Aggregator, Body, End, EndStatus, Event, Network, NetworkInfo, OpenTool, Output,
    REFRESH_PERIOD, Rich, Signal, Start, Usage, terminal_size,
};
use crate::pi;
use crate::process_tree;
use crate::sandbox;
use crate::session::{Session, find_executable, parse_env_args, read_env_file, resolve_path_dirs};
use crate::signal::{SharedWriter, Signals};

const STDERR_TAIL_CHARS: usize = 500;
const STDERR_TAIL_BYTES: usize = STDERR_TAIL_CHARS * 4 + 3;
const TEMP_ENV_VARS: [&str; 3] = ["TMPDIR", "TMP", "TEMP"];
const DRAIN_PERIOD: Duration = Duration::from_secs(1);
const DRY_RUN_TEMPDIR: &str = "<tempdir>";
const TEMPDIR_PREFIX: &str = "agentrun-";

pub type AdapterLookup = dyn Fn(Runtime) -> Box<dyn Adapter>;

pub struct Caller {
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub stdin: Box<dyn Read>,
    pub stdin_is_terminal: bool,
    pub stdout: SharedWriter,
    pub stdout_is_terminal: bool,
    pub stderr: Arc<Mutex<dyn Write + Send>>,
    pub stderr_is_terminal: bool,
    pub signals: Signals,
}

impl Caller {
    pub fn var(&self, key: &str) -> Option<&OsStr> {
        self.env
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_os_str())
    }

    pub fn print_error_line(&self, line: &str) {
        if let Ok(mut stderr) = self.stderr.lock() {
            let _ = writeln!(stderr, "{line}");
            let _ = stderr.flush();
        }
    }

    fn emit(&self, output: &mut Output, event: &Event, open_tool: OpenTool) {
        self.with_stdout(|stdout| output.write(stdout, event, open_tool));
    }

    fn with_stdout(&self, write: impl FnOnce(&mut dyn Write)) {
        if let Ok(mut stdout) = self.stdout.lock() {
            write(&mut *stdout);
        }
    }

    pub fn print_lines(&self, lines: &[String]) {
        if let Ok(mut stdout) = self.stdout.lock() {
            for line in lines {
                let _ = writeln!(stdout, "{line}");
            }
            let _ = stdout.flush();
        }
    }
}

pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<Signal>,
    pub timed_out: bool,
}

struct Ready {
    invocation: Invocation,
    adapter: Box<dyn Adapter>,
    executable: PathBuf,
    raw: Option<(PathBuf, File)>,
}

pub fn run(mut caller: Caller, adapters: &AdapterLookup) -> u8 {
    let started = Instant::now();
    let fallback_format =
        prescan_format(&caller.args).unwrap_or(default_format(caller.stdout_is_terminal));
    let cli = match Cli::try_parse_from(&caller.args) {
        Ok(cli) => cli,
        Err(error) => match error.kind() {
            ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion => {
                caller.signals.finishing();
                caller.print_lines(&[error.render().to_string().trim_end().to_string()]);
                return 0;
            }
            _ => {
                let detail = usage_error_detail(&error.render().to_string());
                if doctor::selected(&caller.args) {
                    caller.signals.finishing();
                    caller.print_error_line(&format!("agentrun: {detail}"));
                    return doctor::USAGE_EXIT_CODE;
                }
                return reject(&mut caller, fallback_format, &detail, started);
            }
        },
    };
    let (runtime, args) = match cli.command.into_parsed() {
        Parsed::Run(runtime, args) => (runtime, args),
        Parsed::Doctor(args) => return doctor::run(caller, args, doctor::COMMAND_TIMEOUT),
        Parsed::Connect(args) => return doctor::connect(&caller, &args),
    };
    let format = args.format.unwrap_or(fallback_format);
    if let Err(code) = caller
        .signals
        .prepare(Arc::clone(&caller.stdout), format, started)
    {
        return code;
    }
    let mut ready = match prepare(&mut caller, runtime, args, format, adapters) {
        Ok(ready) => ready,
        Err(detail) => return reject(&mut caller, format, &detail, started),
    };
    let proxy_needed = network::proxy_needed(
        ready.invocation.runtime,
        ready.invocation.args.network,
        ready.invocation.sandbox.runs(),
    );
    let codex_login = (ready.invocation.runtime == Runtime::Codex)
        .then(|| codex::login_file(&ready.invocation.session, &ready.invocation.cwd));
    if ready.invocation.args.dry_run {
        ready.invocation.tempdir = PathBuf::from(DRY_RUN_TEMPDIR);
        ready.invocation.proxy = proxy_needed.then(|| ProxyEndpoint {
            port: None,
            socket: ready.invocation.tempdir.join(network::SOCKET_FILE),
        });
        if let Some(login) = &codex_login {
            if let Err(detail) = codex::check_login(&ready.invocation.session, login) {
                return reject(&mut caller, format, &detail, started);
            }
            ready.invocation.codex_home = Some(codex::home_path(&ready.invocation.tempdir));
        }
        let launch = match ready.adapter.launch(&ready.executable, &ready.invocation) {
            Ok(launch) => launch,
            Err(detail) => return reject(&mut caller, format, &detail, started),
        };
        caller.signals.finishing();
        caller.print_lines(&plan_lines(
            &launch,
            &ready.invocation.session,
            &ready.invocation.prompt,
        ));
        return 0;
    }
    let credential = match write_session_credential(
        ready.invocation.runtime,
        &ready.invocation.session,
        &ready.invocation.cwd,
    ) {
        Ok(credential) => credential,
        Err(detail) => return reject(&mut caller, format, &detail, started),
    };
    let credential_line = match credential {
        Some(report) => format!(
            "credentials: {} -> {} ({})",
            report.variable,
            report.path.display(),
            report.status.name()
        ),
        None => "credentials: (none)".to_string(),
    };
    let hold = caller.signals.hold();
    let tempdir = match create_tempdir(&caller, TEMPDIR_PREFIX) {
        Ok(tempdir) => tempdir,
        Err(detail) => return reject(&mut caller, format, &detail, started),
    };
    caller
        .signals
        .tempdir(tempdir.path().to_path_buf(), ready.invocation.args.debug);
    drop(hold);
    ready.invocation.tempdir = tempdir.path().to_path_buf();
    let _codex_home = match &codex_login {
        Some(login) => {
            let hold = caller.signals.hold();
            let home = codex::home_path(tempdir.path());
            if let Err(detail) = codex::check_login(&ready.invocation.session, login)
                .and_then(|()| codex::create_home(&home, login))
            {
                return reject(&mut caller, format, &detail, started);
            }
            caller
                .signals
                .tempdir(home.clone(), ready.invocation.args.debug);
            drop(hold);
            ready.invocation.codex_home = Some(home.clone());
            Some(PrivateDir {
                path: home,
                keep: ready.invocation.args.debug,
            })
        }
        None => None,
    };
    let mut proxy = None;
    if proxy_needed {
        match FilterProxy::bind(tempdir.path()) {
            Ok(bound) => {
                ready.invocation.proxy = Some(bound.endpoint());
                proxy = Some(bound);
            }
            Err(error) => {
                return reject(
                    &mut caller,
                    format,
                    &format!("cannot start the filter proxy: {error}"),
                    started,
                );
            }
        }
    }
    let launch = match ready.adapter.launch(&ready.executable, &ready.invocation) {
        Ok(launch) => launch,
        Err(detail) => return reject(&mut caller, format, &detail, started),
    };
    let mut plan = plan_lines(&launch, &ready.invocation.session, &ready.invocation.prompt);
    plan.push(credential_line);
    execute(caller, ready, launch, plan, tempdir, proxy, started)
}

struct PrivateDir {
    path: PathBuf,
    keep: bool,
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn reject(caller: &mut Caller, format: Format, detail: &str, started: Instant) -> u8 {
    caller.signals.finishing();
    let end = Event::now(Body::End(End {
        status: EndStatus::Rejected,
        exit_code: None,
        detail: detail.to_string(),
        duration_ms: elapsed_ms(started),
        usage: Usage::default(),
        result: None,
    }));
    caller.emit(&mut Output::new(format, SandboxMode::On, ""), &end, &|_| {
        None
    });
    caller.print_error_line(&format!("agentrun: {detail}"));
    EndStatus::Rejected.exit_code()
}

fn prepare(
    caller: &mut Caller,
    runtime: Runtime,
    args: RunArgs,
    format: Format,
    adapters: &AdapterLookup,
) -> Result<Ready, String> {
    if args.max_turns.is_some() && runtime != Runtime::ClaudeCode {
        return Err("--max-turns is only supported by claude-code".to_string());
    }
    if let (Runtime::Pi, Some(model)) = (runtime, &args.model) {
        pi::parse_model(model)?;
    }
    let allow_hosts = network::check_usage(args.network, &args.allow_host)?;
    let cwd = resolve_cwd(args.cwd.as_deref())?;
    let prompt = read_prompt(caller, &args)?;
    let env_args = parse_env_args(&args.env, &caller.env)?;
    let path_dirs = resolve_path_dirs(&args.path)?;
    let raw = match &args.raw {
        Some(path) => Some((path.clone(), create_raw(path)?)),
        None => None,
    };
    let env_files = args
        .env_file
        .iter()
        .map(|path| read_env_file(path))
        .collect::<Result<Vec<_>, _>>()?;
    let session = Session::assemble(runtime, &caller.env, &path_dirs, &env_files, &env_args);
    let mode = sandbox::resolve_mode(args.sandbox, session.sandbox.as_deref())?;
    let executable = find_executable(runtime.executable(), &session.path, &cwd)
        .ok_or_else(|| format!("{} not found in PATH", runtime.executable()))?;
    let sandbox = sandbox::check(mode, runtime, &session, &cwd, &caller.signals)?;
    if args.debug {
        caller.print_error_line(&format!("[debug] sandbox: {}", sandbox.description));
    }
    let adapter = adapters(runtime);
    let invocation = Invocation {
        runtime,
        args,
        cwd,
        prompt,
        format,
        sandbox,
        tempdir: PathBuf::new(),
        session,
        allow_hosts,
        proxy: None,
        codex_home: None,
    };
    Ok(Ready {
        invocation,
        adapter,
        executable,
        raw,
    })
}

fn resolve_cwd(cwd: Option<&Path>) -> Result<PathBuf, String> {
    let cwd = match cwd {
        Some(cwd) => cwd.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|error| format!("cannot read the current directory: {error}"))?,
    };
    if !cwd.is_dir() {
        return Err(format!(
            "--cwd {} is not an existing directory",
            cwd.display()
        ));
    }
    std::path::absolute(&cwd).map_err(|error| format!("--cwd {}: {error}", cwd.display()))
}

fn read_prompt(caller: &mut Caller, args: &RunArgs) -> Result<String, String> {
    let prompt = if let Some(prompt) = &args.prompt {
        prompt.clone()
    } else if let Some(path) = &args.prompt_file {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read prompt file {}: {error}", path.display()))?;
        String::from_utf8(bytes)
            .map_err(|_| format!("prompt file {} is not valid UTF-8", path.display()))?
    } else if caller.stdin_is_terminal {
        return Err(
            "no prompt given: use --prompt, --prompt-file or pipe the prompt to standard input"
                .to_string(),
        );
    } else {
        let mut bytes = Vec::new();
        caller
            .stdin
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read the prompt from standard input: {error}"))?;
        String::from_utf8(bytes)
            .map_err(|_| "the prompt from standard input is not valid UTF-8".to_string())?
    };
    if prompt.trim().is_empty() {
        return Err("the prompt is empty".to_string());
    }
    Ok(prompt)
}

fn create_raw(path: &Path) -> Result<File, String> {
    File::create(path)
        .map_err(|error| format!("cannot create raw file {}: {error}", path.display()))
}

pub fn create_tempdir(caller: &Caller, prefix: &str) -> Result<TempDir, String> {
    let base = match caller.var("TMPDIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from("/tmp"),
    };
    tempfile::Builder::new()
        .prefix(prefix)
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(&base)
        .map_err(|error| {
            format!(
                "cannot create the session temporary directory in {}: {error}",
                base.display()
            )
        })
}

fn plan_lines(launch: &Launch, session: &Session, prompt: &str) -> Vec<String> {
    let command = launch
        .argv
        .iter()
        .map(|arg| {
            if arg == prompt {
                shell_quote(&format!("<prompt {} bytes>", prompt.len()))
            } else {
                shell_quote(arg)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    vec![
        format!("command: {command}"),
        format!("PATH: {}", session.path.to_string_lossy()),
        format!("set: {}", name_list(&session.set)),
        format!("removed: {}", name_list(&session.removed)),
        format!(
            "agentrun variables: {}",
            name_list(&session.agentrun_variables)
        ),
    ]
}

fn name_list(names: &[String]) -> String {
    if names.is_empty() {
        "(none)".to_string()
    } else {
        names.join(", ")
    }
}

pub fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:=@%+,-".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

fn execute(
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
        let (upstream, notes) = Upstream::from_env(&invocation.session.env);
        if debug {
            for note in notes {
                caller.print_error_line(&format!("[debug] {note}"));
            }
        }
        let policy = Policy {
            mode: invocation.args.network,
            rules: invocation.allow_hosts.clone(),
            service_hosts: launch.service_hosts.clone(),
        };
        let reporter = sender.clone();
        proxy.serve(policy, upstream, move |network| {
            let _ = reporter.send(Message::Network(network));
        });
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
    let mut output = if invocation.format == Format::Rich && caller.stdout_is_terminal {
        let color = caller.var("NO_COLOR").is_none_or(|value| value.is_empty());
        Output::Rich(Box::new(Rich::new(
            invocation.sandbox.mode,
            &invocation.sandbox.reason,
            color,
            Box::new(terminal_size),
        )))
    } else {
        Output::new(
            invocation.format,
            invocation.sandbox.mode,
            &invocation.sandbox.reason,
        )
    };
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
            let end = Event::now(Body::End(End {
                status: EndStatus::Failed,
                exit_code: None,
                detail: format!("failed to start {program}: {error}"),
                duration_ms: elapsed_ms(started),
                usage: Usage::default(),
                result: None,
            }));
            caller.emit(&mut output, &end, &|_| None);
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
        sandbox: invocation.sandbox.kind,
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
    caller.emit(&mut output, &start, &|_| None);
    let mut aggregator = Aggregator::new(adapter.echoes_prompt());
    for event in aggregator.begin(&invocation.prompt) {
        caller.emit(&mut output, &event, &|_| None);
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
            let open_tool = |agent: Option<&str>| aggregator.open_tool(agent);
            match input {
                Input::Network(network) => {
                    caller.emit(&mut output, &Event::now(Body::Network(network)), &open_tool);
                }
                Input::Stderr(bytes) => {
                    caller.with_stdout(|stdout| output.stderr(stdout, bytes, &open_tool));
                }
                _ => caller.with_stdout(|stdout| output.refresh(stdout, &open_tool)),
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
        let open_tool = |agent: Option<&str>| aggregator.open_tool(agent);
        for event in translated.events {
            caller.emit(&mut output, &event, &open_tool);
        }
        if debug {
            for line in translated.debug {
                let line = format!("[debug] {line}\n");
                if rich_stderr {
                    caller.with_stdout(|stdout| output.stderr(stdout, line.as_bytes(), &open_tool));
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
    let (events, status) = conclude(aggregator, adapter.as_mut(), &exit, &stderr_tail, started);
    for event in &events {
        caller.emit(&mut output, event, &|_| None);
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

pub struct Translated {
    pub events: Vec<Event>,
    pub debug: Vec<String>,
    pub terminate: bool,
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
    push_records(records, aggregator)
}

fn push_records(records: Vec<Record>, aggregator: &mut Aggregator) -> Translated {
    let mut translated = Translated {
        events: Vec::new(),
        debug: Vec::new(),
        terminate: false,
    };
    for record in records {
        match record {
            Record::Terminate => translated.terminate = true,
            Record::Debug(line) => translated.debug.push(line),
            record => translated.events.extend(aggregator.push(record)),
        }
    }
    translated
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
    tail_chars(stderr.trim_end(), STDERR_TAIL_CHARS)
}

fn tail_chars(text: &str, count: usize) -> String {
    let skip = text.chars().count().saturating_sub(count);
    text.chars().skip(skip).collect()
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
    let mut events = push_records(adapter.after_exit(), &mut aggregator).events;
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

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_leaves_plain_arguments() {
        assert_eq!(shell_quote("/usr/bin/claude"), "/usr/bin/claude");
        assert_eq!(shell_quote("--mode=json"), "--mode=json");
        assert_eq!(shell_quote("a_b.c:d@e%f+g,h-i"), "a_b.c:d@e%f+g,h-i");
    }

    #[test]
    fn shell_quote_wraps_other_arguments() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("<prompt 12 bytes>"), "'<prompt 12 bytes>'");
        assert_eq!(shell_quote("修复"), "'修复'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn tail_keeps_last_characters() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("ab", 3), "ab");
        assert_eq!(tail_chars("é修复", 2), "修复");
    }
}
