mod execute;
mod signal;

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use clap::Parser;
use clap::error::ErrorKind as ClapErrorKind;
use tempfile::TempDir;

use crate::cli::{
    Cli, ConnectArgs, DoctorArgs, Format, Parsed, RunArgs, Runtime, default_format, prescan_format,
    usage_error_detail,
};
use crate::credential::write_session_credential;
use crate::doctor;
use crate::network::{self, FilterProxy, ProxyEndpoint};
use crate::output::{End, EndStatus, Event, Output, write_end};
use crate::runtime::{Adapter, Invocation, Launch};
use crate::sandbox;
use crate::session::{Session, find_executable, parse_env_args, read_env_file, resolve_path_dirs};
use execute::execute;
use signal::SharedWriter;

pub use execute::{Exit, conclude, stderr_tail, translate_line};
pub use signal::Signals;

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

    fn emit(&self, output: &mut Output, event: &Event) {
        self.with_stdout(|stdout| output.write(stdout, event));
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

enum Command {
    Run(Runtime, RunArgs),
    Doctor(DoctorArgs),
    Connect(ConnectArgs),
    Exit(u8),
    UsageError(String),
}

enum Prepared {
    DryRun(Vec<String>),
    Execute(Box<Resources>),
}

struct Ready {
    invocation: Invocation,
    adapter: Box<dyn Adapter>,
    executable: PathBuf,
    raw: Option<(PathBuf, File)>,
}

struct Resources {
    invocation: Invocation,
    adapter: Box<dyn Adapter>,
    launch: Launch,
    plan: Vec<String>,
    raw: Option<(PathBuf, File)>,
}

#[derive(Default)]
struct Guards {
    proxy: Option<FilterProxy>,
    private_dirs: Vec<OwnedDir>,
    tempdir: Option<TempDir>,
}

struct OwnedDir {
    path: PathBuf,
    keep: bool,
}

impl Drop for OwnedDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

pub fn run(mut caller: Caller, adapters: &AdapterLookup) -> u8 {
    let started = Instant::now();
    let fallback_format =
        prescan_format(&caller.args).unwrap_or(default_format(caller.stdout_is_terminal));
    let mut guards = Guards::default();
    let (format, prepared) = match parse_command(&caller) {
        Command::Run(runtime, args) => {
            let format = args.format.unwrap_or(fallback_format);
            if let Err(code) = caller
                .signals
                .prepare(Arc::clone(&caller.stdout), format, started)
            {
                return code;
            }
            let prepared = set_up(&mut caller, runtime, args, format, adapters, &mut guards);
            (format, prepared)
        }
        Command::UsageError(detail) => (fallback_format, Err(detail)),
        Command::Doctor(args) => return doctor::run(caller, args, doctor::COMMAND_TIMEOUT),
        Command::Connect(args) => return doctor::connect(&caller, &args),
        Command::Exit(code) => return code,
    };
    let outcome = match prepared {
        Ok(Prepared::DryRun(lines)) => {
            caller.signals.finishing();
            caller.print_lines(&lines);
            Ok(0)
        }
        Ok(Prepared::Execute(resources)) => execute(&mut caller, *resources, &mut guards, started),
        Err(detail) => Err(detail),
    };
    match outcome {
        Ok(code) => code,
        Err(detail) => reject(&mut caller, format, &detail, started),
    }
}

fn parse_command(caller: &Caller) -> Command {
    let cli = match Cli::try_parse_from(&caller.args) {
        Ok(cli) => cli,
        Err(error) => {
            return match error.kind() {
                ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion => {
                    caller.signals.finishing();
                    caller.print_lines(&[error.render().to_string().trim_end().to_string()]);
                    Command::Exit(0)
                }
                _ => {
                    let detail = usage_error_detail(&error.render().to_string());
                    if doctor::selected(&caller.args) {
                        caller.signals.finishing();
                        caller.print_error_line(&format!("agentrun: {detail}"));
                        Command::Exit(doctor::USAGE_EXIT_CODE)
                    } else {
                        Command::UsageError(detail)
                    }
                }
            };
        }
    };
    match cli.command.into_parsed() {
        Parsed::Run(runtime, args) => Command::Run(runtime, args),
        Parsed::Doctor(args) => Command::Doctor(args),
        Parsed::Connect(args) => Command::Connect(args),
    }
}

fn set_up(
    caller: &mut Caller,
    runtime: Runtime,
    args: RunArgs,
    format: Format,
    adapters: &AdapterLookup,
    guards: &mut Guards,
) -> Result<Prepared, String> {
    let mut ready = prepare(caller, runtime, args, format, adapters)?;
    let proxy_needed = network::proxy_needed(
        ready.invocation.runtime,
        ready.invocation.args.network,
        ready.invocation.sandbox.runs(),
    );
    if ready.invocation.args.dry_run {
        return dry_run(&mut ready, proxy_needed).map(Prepared::DryRun);
    }
    let credential = write_session_credential(
        ready.invocation.runtime,
        &ready.invocation.session,
        &ready.invocation.cwd,
    )?;
    let credential_line = match credential {
        Some(report) => format!(
            "credentials: {} -> {} ({})",
            report.variable,
            report.path.display(),
            report.status.name()
        ),
        None => "credentials: (none)".to_string(),
    };
    let debug = ready.invocation.args.debug;
    let hold = caller.signals.hold();
    let tempdir =
        create_tempdir(caller, TEMPDIR_PREFIX).inspect_err(|_| caller.signals.finishing())?;
    caller.signals.tempdir(tempdir.path().to_path_buf(), debug);
    drop(hold);
    ready.invocation.tempdir = tempdir.path().to_path_buf();
    guards.tempdir = Some(tempdir);
    if proxy_needed {
        let bound = FilterProxy::bind(&ready.invocation.tempdir)?;
        ready.invocation.proxy = Some(bound.endpoint());
        guards.proxy = Some(bound);
    }
    let hold = caller.signals.hold();
    let launch = ready
        .adapter
        .launch(&ready.executable, &ready.invocation)
        .inspect_err(|_| caller.signals.finishing())?;
    for dir in &launch.private_dirs {
        caller.signals.tempdir(dir.path.clone(), debug);
        guards.private_dirs.push(OwnedDir {
            path: dir.path.clone(),
            keep: debug,
        });
    }
    drop(hold);
    let mut plan = plan_lines(&launch, &ready.invocation.session, &ready.invocation.prompt);
    plan.push(credential_line);
    let Ready {
        invocation,
        adapter,
        raw,
        ..
    } = ready;
    Ok(Prepared::Execute(Box::new(Resources {
        invocation,
        adapter,
        launch,
        plan,
        raw,
    })))
}

fn dry_run(ready: &mut Ready, proxy_needed: bool) -> Result<Vec<String>, String> {
    ready.invocation.tempdir = PathBuf::from(DRY_RUN_TEMPDIR);
    ready.invocation.proxy = proxy_needed.then(|| ProxyEndpoint {
        port: None,
        socket: ready.invocation.tempdir.join(network::SOCKET_FILE),
    });
    let launch = ready.adapter.launch(&ready.executable, &ready.invocation)?;
    Ok(plan_lines(
        &launch,
        &ready.invocation.session,
        &ready.invocation.prompt,
    ))
}

fn reject(caller: &mut Caller, format: Format, detail: &str, started: Instant) -> u8 {
    caller.signals.finishing();
    caller.with_stdout(|out| {
        write_end(
            out,
            format,
            End::early(EndStatus::Rejected, detail.to_string(), started),
        )
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
    let adapter = adapters(runtime);
    adapter.check_args(&args)?;
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
    let sandbox = sandbox::check(mode, runtime, &session, &cwd, &|pid| {
        caller.signals.checking(pid)
    })?;
    if args.debug {
        caller.print_error_line(&format!("[debug] sandbox: {}", sandbox.description));
    }
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

pub(crate) fn create_tempdir(caller: &Caller, prefix: &str) -> Result<TempDir, String> {
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

pub(crate) fn shell_quote(arg: &str) -> String {
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
}
