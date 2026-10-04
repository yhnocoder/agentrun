use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use clap::Parser;
use clap::error::ErrorKind as ClapErrorKind;
use serde_json::Value;
use tempfile::TempDir;

use crate::adapter::{Adapter, Launch};
use crate::aggregate::Aggregator;
use crate::cli::{
    Cli, Format, RunArgs, Runtime, SandboxMode, default_format, prescan_format, usage_error_detail,
};
use crate::credential::write_session_credential;
use crate::event::{Body, End, EndStatus, Event, NetworkInfo, SandboxKind, Start};
use crate::output::Output;
use crate::session::{Session, find_executable, parse_env_args, read_env_file, resolve_path_dirs};
use crate::usage::Usage;

const STDERR_TAIL_CHARS: usize = 500;
const STDERR_TAIL_BYTES: usize = STDERR_TAIL_CHARS * 4 + 3;
const SANDBOX_UNAVAILABLE_REASON: &str = "not implemented in this build";
const TEMP_ENV_VARS: [&str; 3] = ["TMPDIR", "TMP", "TEMP"];

pub type AdapterLookup = dyn Fn(Runtime) -> Option<Box<dyn Adapter>>;

pub struct Caller {
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub stdin: Box<dyn Read>,
    pub stdin_is_terminal: bool,
    pub stdout: Box<dyn Write>,
    pub stdout_is_terminal: bool,
    pub stderr: Arc<Mutex<dyn Write + Send>>,
}

impl Caller {
    fn var(&self, key: &str) -> Option<&OsStr> {
        self.env
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_os_str())
    }

    fn print_error_line(&self, line: &str) {
        if let Ok(mut stderr) = self.stderr.lock() {
            let _ = writeln!(stderr, "{line}");
            let _ = stderr.flush();
        }
    }
}

pub struct Invocation {
    pub runtime: Runtime,
    pub args: RunArgs,
    pub cwd: PathBuf,
    pub prompt: String,
    pub format: Format,
}

struct Ready {
    invocation: Invocation,
    adapter: Box<dyn Adapter>,
    launch: Launch,
    raw: Option<(PathBuf, File)>,
    session: Session,
}

pub fn run(mut caller: Caller, adapters: &AdapterLookup) -> u8 {
    let started = Instant::now();
    let fallback_format =
        prescan_format(&caller.args).unwrap_or(default_format(caller.stdout_is_terminal));
    let cli = match Cli::try_parse_from(&caller.args) {
        Ok(cli) => cli,
        Err(error) => match error.kind() {
            ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion => {
                let _ = write!(caller.stdout, "{}", error.render());
                let _ = caller.stdout.flush();
                return 0;
            }
            _ => {
                let detail = usage_error_detail(&error.render().to_string());
                return reject(&mut caller, fallback_format, &detail, started);
            }
        },
    };
    let (runtime, args) = cli.command.into_parts();
    let format = args.format.unwrap_or(fallback_format);
    let ready = match prepare(&mut caller, runtime, args, format, adapters) {
        Ok(ready) => ready,
        Err(detail) => return reject(&mut caller, format, &detail, started),
    };
    let mut plan = plan_lines(&ready);
    if ready.invocation.args.dry_run {
        for line in &plan {
            let _ = writeln!(caller.stdout, "{line}");
        }
        let _ = caller.stdout.flush();
        return 0;
    }
    let credential = match write_session_credential(
        ready.invocation.runtime,
        &ready.session,
        &ready.invocation.cwd,
    ) {
        Ok(credential) => credential,
        Err(detail) => return reject(&mut caller, format, &detail, started),
    };
    plan.push(match credential {
        Some(report) => format!(
            "credentials: {} -> {} ({})",
            report.variable,
            report.path.display(),
            report.status.name()
        ),
        None => "credentials: (none)".to_string(),
    });
    let tempdir = match create_tempdir(&caller) {
        Ok(tempdir) => tempdir,
        Err(detail) => return reject(&mut caller, format, &detail, started),
    };
    execute(caller, ready, plan, tempdir, started)
}

fn reject(caller: &mut Caller, format: Format, detail: &str, started: Instant) -> u8 {
    let end = Event::now(Body::End(End {
        status: EndStatus::Rejected,
        exit_code: None,
        detail: detail.to_string(),
        duration_ms: elapsed_ms(started),
        usage: Usage::default(),
        result: None,
    }));
    Output::new(format, SandboxMode::On, "").write(&mut caller.stdout, &end);
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
    let executable = find_executable(runtime.executable(), &session.path, &cwd)
        .ok_or_else(|| format!("{} not found in PATH", runtime.executable()))?;
    if args.sandbox == SandboxMode::On {
        return Err(format!(
            "sandbox is not available: {SANDBOX_UNAVAILABLE_REASON}. Use --sandbox relax or --sandbox off"
        ));
    }
    let adapter = adapters(runtime).ok_or_else(|| {
        format!(
            "{} support is not implemented in this build",
            runtime.name()
        )
    })?;
    let invocation = Invocation {
        runtime,
        args,
        cwd,
        prompt,
        format,
    };
    let launch = adapter.launch(&executable, &invocation);
    Ok(Ready {
        invocation,
        adapter,
        launch,
        raw,
        session,
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

fn create_tempdir(caller: &Caller) -> Result<TempDir, String> {
    let base = match caller.var("TMPDIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from("/tmp"),
    };
    tempfile::Builder::new()
        .prefix("agentrun-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(&base)
        .map_err(|error| {
            format!(
                "cannot create the session temporary directory in {}: {error}",
                base.display()
            )
        })
}

fn plan_lines(ready: &Ready) -> Vec<String> {
    let prompt = &ready.invocation.prompt;
    let command = ready
        .launch
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
    let session = &ready.session;
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
    plan: Vec<String>,
    tempdir: TempDir,
    started: Instant,
) -> u8 {
    let Ready {
        invocation,
        mut adapter,
        launch,
        raw,
        session,
    } = ready;
    let debug = invocation.args.debug;
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
        if let Some((path, _)) = &raw {
            caller.print_error_line(&format!("[debug] raw: {}", path.display()));
        }
    }
    let mut raw = raw.map(|(_, file)| file);
    let mut output = Output::new(
        invocation.format,
        invocation.args.sandbox,
        SANDBOX_UNAVAILABLE_REASON,
    );

    let (program, program_args) = launch
        .argv
        .split_first()
        .expect("adapter argv starts with the executable");
    let mut command = Command::new(program);
    command
        .args(program_args)
        .current_dir(&invocation.cwd)
        .env_clear()
        .envs(&session.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in TEMP_ENV_VARS {
        command.env(key, tempdir.path());
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let end = Event::now(Body::End(End {
                status: EndStatus::Failed,
                exit_code: None,
                detail: format!("failed to start {program}: {error}"),
                duration_ms: elapsed_ms(started),
                usage: Usage::default(),
                result: None,
            }));
            output.write(&mut caller.stdout, &end);
            finish_tempdir(tempdir, debug);
            return EndStatus::Failed.exit_code();
        }
    };

    let start = Event::now(Body::Start(Start {
        runtime: invocation.runtime,
        sandbox: SandboxKind::None,
        network: NetworkInfo {
            mode: invocation.args.network,
            allow: invocation.args.allow_host.clone(),
            enforced: false,
        },
        model: invocation.args.model.clone(),
        cwd: invocation.cwd.to_string_lossy().into_owned(),
        argv: launch
            .argv
            .iter()
            .filter(|arg| **arg != invocation.prompt)
            .cloned()
            .collect(),
        env: session.set.clone(),
    }));
    output.write(&mut caller.stdout, &start);
    let mut aggregator = Aggregator::new(adapter.echoes_prompt());
    for event in aggregator.begin(&invocation.prompt) {
        output.write(&mut caller.stdout, &event);
    }

    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = launch.stdin;
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let stderr = child.stderr.take().expect("stderr is piped");
    let sink = Arc::clone(&caller.stderr);
    let forwarder = thread::spawn(move || forward_stderr(stderr, sink));

    let stdout = child.stdout.take().expect("stdout is piped");
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
        let content = line.strip_suffix(b"\n").unwrap_or(&line);
        if let Some(file) = raw.as_mut() {
            let mut bytes = Vec::with_capacity(content.len() + 1);
            bytes.extend_from_slice(content);
            bytes.push(b'\n');
            let _ = file.write_all(&bytes);
        }
        for event in translate_line(content, adapter.as_mut(), &mut aggregator) {
            output.write(&mut caller.stdout, &event);
        }
    }

    let exit_code = child.wait().ok().and_then(|status| status.code());
    let _ = writer.join();
    let tail = forwarder.join().unwrap_or_default();
    let stderr_tail = tail_chars(String::from_utf8_lossy(&tail).trim_end(), STDERR_TAIL_CHARS);
    let (events, status) = conclude(
        aggregator,
        adapter.as_ref(),
        exit_code,
        &stderr_tail,
        started,
    );
    for event in &events {
        output.write(&mut caller.stdout, event);
    }
    drop(raw);
    finish_tempdir(tempdir, debug);
    status.exit_code()
}

pub fn translate_line(
    line: &[u8],
    adapter: &mut dyn Adapter,
    aggregator: &mut Aggregator,
) -> Vec<Event> {
    match serde_json::from_slice::<Value>(line) {
        Ok(value) => adapter
            .translate(&value)
            .into_iter()
            .flat_map(|record| aggregator.push(record))
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn forward_stderr(mut stderr: impl Read, sink: Arc<Mutex<dyn Write + Send>>) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = match stderr.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if let Ok(mut sink) = sink.lock() {
            let _ = sink.write_all(&buffer[..count]);
            let _ = sink.flush();
        }
        tail.extend_from_slice(&buffer[..count]);
        if tail.len() > STDERR_TAIL_BYTES {
            tail.drain(..tail.len() - STDERR_TAIL_BYTES);
        }
    }
    tail
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
    aggregator: Aggregator,
    adapter: &dyn Adapter,
    exit_code: Option<i32>,
    stderr_tail: &str,
    started: Instant,
) -> (Vec<Event>, EndStatus) {
    let (mut events, summary) = aggregator.finish();
    let (status, detail) = match adapter.failure(exit_code, stderr_tail) {
        Some(detail) => (
            EndStatus::Failed,
            failure_detail(detail, stderr_tail, adapter.runtime()),
        ),
        None => (EndStatus::Finished, String::new()),
    };
    events.push(Event::now(Body::End(End {
        status,
        exit_code,
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
