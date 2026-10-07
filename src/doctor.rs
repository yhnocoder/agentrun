use std::ffi::OsString;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::cli::{ConnectArgs, DoctorArgs, Format, NetworkMode, RunArgs, Runtime, SandboxMode};
use crate::credential::write_session_credential;
use crate::network::{
    self, FilterProxy, HostRule, Policy, ProxyAddress, ProxyEndpoint, check_usage, in_cidr,
    proxy_environment,
};
use crate::output::{Network, NetworkReason, Signal};
use crate::process_tree::{kill_group_members, wait_for_exit};
use crate::run::{Caller, create_tempdir, shell_quote};
use crate::runtime::codex;
use crate::runtime::pi::{self, Model, Pi};
use crate::runtime::{Adapter, Invocation};
use crate::sandbox::{
    self, BWRAP_PREFIX, CANNOT_START_HINT, Sandbox, UNAVAILABLE_PREFIX, Wrapped, Wrapper,
};
use crate::session::{
    Session, find_executable, home_placeholder, parse_env_args, read_env_file, resolve_path_dirs,
};

pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const USAGE_EXIT_CODE: u8 = 2;
const FAIL_EXIT_CODE: u8 = 1;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_RETRY: Duration = Duration::from_millis(50);
const POLL: Duration = Duration::from_millis(10);
const DRAIN: Duration = Duration::from_secs(1);
const TEMPDIR_PREFIX: &str = "agentrun-doctor-";
const PROMPT: &str = "agentrun doctor";
const ORDER: [Runtime; 3] = [Runtime::ClaudeCode, Runtime::Codex, Runtime::Pi];
const SANDBOX_SCRIPT: &str =
    r#"echo ok > "$1/inside.txt"; echo ok > "$2/outside.txt" 2>/dev/null; echo done"#;
const INSIDE_FILE: &str = "inside.txt";
const OUTSIDE_FILE: &str = "outside.txt";
const CODEX_HOME_DIR: &str = "codex-home";
const PI_UNKNOWN_MODEL_PREFIX: &str = "Warning: Model ";
const PI_UNKNOWN_MODEL_TEXT: &str = "not found for provider";
const SERVICE_PORT: u16 = 443;
const DENIED_HOST: &str = "doctor-check.invalid";
const PRIVATE_HOST: &str = "169.254.169.254";
const PRIVATE_PORT: u16 = 80;
const CLAUDE_CODE_SERVICE_HOST: &str = "api.anthropic.com";
const CODEX_SERVICE_HOST: &str = "chatgpt.com";
const PI_FALLBACK_SERVICE_HOST: &str = "api.anthropic.com";
const FAKE_IP_RANGE: (Ipv4Addr, u8) = (Ipv4Addr::new(198, 18, 0, 0), 15);
const FAKE_IP_HINT: &str = "codex's network proxy treats this range as private; turn off the fake-ip mode of the local proxy software";
const TOKEN_VARIABLE: &str = "CLAUDE_CODE_OAUTH_TOKEN";
const NOT_LOGGED_IN: &str =
    "not logged in. Run claude login, or set CLAUDE_CODE_OAUTH_TOKEN (from claude setup-token)";
const CONNECT_PROXY_VARIABLES: [&str; 2] = ["https_proxy", "HTTPS_PROXY"];
const PROXY_ERROR_HEADER: &str = "x-proxy-error";
const CODEX_PROXY_NOT_ALLOWED: &str = "x-proxy-error: blocked-by-allowlist";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Check {
    Executable,
    Login,
    Sandbox,
    Network,
}

impl Check {
    pub fn name(self) -> &'static str {
        match self {
            Check::Executable => "executable",
            Check::Login => "login",
            Check::Sandbox => "sandbox",
            Check::Network => "network",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Fail,
    Skip,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "[ok]",
            Status::Fail => "[fail]",
            Status::Skip => "[skip]",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Item {
    pub runtime: Runtime,
    pub check: Check,
    pub status: Status,
    pub detail: String,
}

impl Item {
    pub fn line(&self) -> String {
        format!(
            "{:<7}{:<13}{:<12}{}",
            self.status.label(),
            self.runtime.name(),
            self.check.name(),
            self.detail
        )
    }
}

pub(crate) fn selected(args: &[OsString]) -> bool {
    matches!(
        args.get(1).and_then(|arg| arg.to_str()),
        Some("doctor" | "doctor-connect")
    )
}

struct Target {
    runtime: Runtime,
    session: Session,
    mode: SandboxMode,
}

struct Plan {
    targets: Vec<Target>,
    allow_hosts: Vec<HostRule>,
    cwd: PathBuf,
}

struct Doctor {
    caller: Caller,
    args: DoctorArgs,
    timeout: Duration,
    tempdir: PathBuf,
    items: Arc<Mutex<Vec<Item>>>,
    allow_hosts: Vec<HostRule>,
    cwd: PathBuf,
    self_exe: PathBuf,
    codex_home: PathBuf,
}

pub fn run(caller: Caller, args: DoctorArgs, timeout: Duration) -> u8 {
    let items: Arc<Mutex<Vec<Item>>> = Arc::default();
    let report = {
        let items = Arc::clone(&items);
        let stdout = Arc::clone(&caller.stdout);
        let json = args.json;
        Box::new(move |_: Signal| {
            if json {
                write_json(&stdout, &items);
            }
        })
    };
    if let Err(code) = caller.signals.doctoring(report) {
        return code;
    }
    let plan = match plan(&caller, &args) {
        Ok(plan) => plan,
        Err(detail) => return usage_error(&caller, &detail),
    };
    for target in &plan.targets {
        if let Err(detail) = write_session_credential(target.runtime, &target.session, &plan.cwd) {
            return usage_error(&caller, &detail);
        }
    }
    let tempdir = match create_tempdir(&caller, TEMPDIR_PREFIX) {
        Ok(tempdir) => tempdir,
        Err(detail) => return usage_error(&caller, &detail),
    };
    caller
        .signals
        .tempdir(tempdir.path().to_path_buf(), args.debug);
    let self_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return usage_error(
                &caller,
                &format!("cannot locate the agentrun executable: {error}"),
            );
        }
    };
    let codex_home = tempdir.path().join(CODEX_HOME_DIR);
    if let Some(codex) = plan
        .targets
        .iter()
        .find(|target| target.runtime == Runtime::Codex)
    {
        let login = codex::login_file(&codex.session, &plan.cwd);
        if let Err(detail) = codex::create_home(&codex_home, &login) {
            return usage_error(&caller, &detail);
        }
    }
    let doctor = Doctor {
        caller,
        args,
        timeout,
        tempdir: tempdir.path().to_path_buf(),
        items,
        allow_hosts: plan.allow_hosts,
        cwd: plan.cwd,
        self_exe,
        codex_home,
    };
    for target in &plan.targets {
        doctor.check_runtime(target);
    }
    let Doctor {
        caller,
        args,
        items,
        ..
    } = doctor;
    caller.signals.finishing();
    if args.json {
        write_json(&caller.stdout, &items);
    }
    if args.debug {
        remove_login_link(tempdir.path());
        caller.print_error_line(&format!("[debug] kept {}", tempdir.path().display()));
        let _ = tempdir.keep();
    }
    let failed = items
        .lock()
        .map(|items| items.iter().any(|item| item.status == Status::Fail))
        .unwrap_or(true);
    if failed { FAIL_EXIT_CODE } else { 0 }
}

fn usage_error(caller: &Caller, detail: &str) -> u8 {
    caller.signals.finishing();
    caller.print_error_line(&format!("agentrun: {detail}"));
    USAGE_EXIT_CODE
}

fn write_json(stdout: &Arc<Mutex<dyn Write + Send>>, items: &Mutex<Vec<Item>>) {
    let items = items.lock().map(|items| items.clone()).unwrap_or_default();
    let text = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
    if let Ok(mut stdout) = stdout.lock() {
        let _ = writeln!(stdout, "{text}");
        let _ = stdout.flush();
    }
}

fn plan(caller: &Caller, args: &DoctorArgs) -> Result<Plan, String> {
    let runtimes: Vec<Runtime> = if args.runtimes.is_empty() {
        ORDER.to_vec()
    } else {
        ORDER
            .into_iter()
            .filter(|runtime| args.runtimes.contains(runtime))
            .collect()
    };
    if let (true, Some(model)) = (runtimes.contains(&Runtime::Pi), &args.model) {
        pi::parse_model(model)?;
    }
    let allow_hosts = check_usage(args.network, &args.allow_host)?;
    let cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;
    let env_args = parse_env_args(&args.env, &caller.env)?;
    let path_dirs = resolve_path_dirs(&args.path)?;
    let env_files = args
        .env_file
        .iter()
        .map(|path| read_env_file(path))
        .collect::<Result<Vec<_>, _>>()?;
    let targets = runtimes
        .into_iter()
        .map(|runtime| {
            let session =
                Session::assemble(runtime, &caller.env, &path_dirs, &env_files, &env_args);
            let mode = sandbox::resolve_mode(args.sandbox, session.sandbox.as_deref())?;
            Ok(Target {
                runtime,
                session,
                mode,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Plan {
        targets,
        allow_hosts,
        cwd,
    })
}

struct Finished {
    exit_code: Option<i32>,
    exit_text: String,
    stdout: Vec<String>,
    stderr: String,
}

impl Finished {
    fn stderr_line(&self) -> Option<&str> {
        first_non_empty(self.stderr.lines())
    }

    fn stdout_line(&self) -> Option<&str> {
        first_non_empty(self.stdout.iter().map(String::as_str))
    }

    fn message(&self) -> String {
        self.stderr_line()
            .or_else(|| self.stdout_line())
            .map(str::to_string)
            .unwrap_or_else(|| self.exit_text.clone())
    }

    fn output(&self) -> String {
        self.stdout_line()
            .or_else(|| self.stderr_line())
            .map(str::to_string)
            .unwrap_or_else(|| self.exit_text.clone())
    }
}

fn first_non_empty<'a>(lines: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    lines.map(str::trim).find(|line| !line.is_empty())
}

struct Run<'a> {
    argv: &'a [String],
    env: &'a [(OsString, OsString)],
    session: &'a Session,
}

impl Doctor {
    fn record(&self, runtime: Runtime, check: Check, status: Status, detail: String) {
        let item = Item {
            runtime,
            check,
            status,
            detail,
        };
        if !self.args.json {
            self.caller.print_lines(&[item.line()]);
        }
        if let Ok(mut items) = self.items.lock() {
            items.push(item);
        }
    }

    fn record_result(&self, runtime: Runtime, check: Check, result: Result<String, String>) {
        match result {
            Ok(detail) => self.record(runtime, check, Status::Ok, detail),
            Err(detail) => self.record(runtime, check, Status::Fail, detail),
        }
    }

    fn execute(
        &self,
        runtime: Runtime,
        check: Check,
        run: Run,
        mut stop_at: impl FnMut(&str) -> bool,
    ) -> Result<Finished, String> {
        let (program, program_args) = run
            .argv
            .split_first()
            .expect("a command starts with its program");
        let name = Path::new(program)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.clone());
        if self.args.debug {
            let quoted = run
                .argv
                .iter()
                .map(|arg| shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" ");
            self.caller.print_error_line(&format!(
                "[debug] {} {} command: {quoted}",
                runtime.name(),
                check.name()
            ));
        }
        let mut child = Command::new(program)
            .args(program_args)
            .current_dir(&self.cwd)
            .env_clear()
            .envs(&run.session.env)
            .envs(run.env.iter().map(|(key, value)| (key, value)))
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot start {name}: {error}"))?;
        let pid = child.id() as i32;
        self.caller.signals.checking(Some(pid));
        let stdout = child.stdout.take().expect("stdout is piped");
        let (line_sender, lines) = mpsc::channel();
        thread::spawn(move || {
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
                if line_sender.send(std::mem::take(&mut line)).is_err() {
                    break;
                }
            }
        });
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let (stderr_sender, stderr_bytes) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            let _ = stderr_sender.send(bytes);
        });
        let deadline = Instant::now() + self.timeout;
        let mut out = Vec::new();
        let mut stdout_open = true;
        let mut timed_out = false;
        loop {
            if stdout_open {
                match lines.recv_timeout(POLL) {
                    Ok(line) => {
                        let text = String::from_utf8_lossy(&line).trim_end().to_string();
                        let stop = stop_at(&text);
                        out.push(text);
                        if stop {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => stdout_open = false,
                }
            } else {
                thread::sleep(POLL);
            }
            if wait_for_exit(pid, false) {
                break;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
        }
        kill_group_members(pid);
        self.caller.signals.checking(None);
        let status = child.wait().ok();
        while let Ok(line) = lines.recv_timeout(DRAIN) {
            out.push(String::from_utf8_lossy(&line).trim_end().to_string());
        }
        let stderr = stderr_bytes.recv_timeout(DRAIN).unwrap_or_default();
        if timed_out {
            return Err(format!(
                "{name} did not finish within {} seconds",
                self.timeout.as_secs()
            ));
        }
        Ok(Finished {
            exit_code: status.and_then(|status| status.code()),
            exit_text: sandbox::exit_text(status.and_then(|status| status.code())),
            stdout: out,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    fn check_runtime(&self, target: &Target) {
        let runtime = target.runtime;
        let executable = find_executable(runtime.executable(), &target.session.path, &self.cwd);
        match &executable {
            Some(path) => self.record(
                runtime,
                Check::Executable,
                Status::Ok,
                path.display().to_string(),
            ),
            None => self.record(
                runtime,
                Check::Executable,
                Status::Fail,
                format!("{} not found in PATH", runtime.executable()),
            ),
        }
        match &executable {
            Some(executable) => {
                let result = match runtime {
                    Runtime::ClaudeCode => self.claude_code_login(target, executable),
                    Runtime::Codex => self.codex_login(target, executable),
                    Runtime::Pi => self.pi_login(target, executable),
                };
                self.record_result(runtime, Check::Login, result);
            }
            None => self.record(
                runtime,
                Check::Login,
                Status::Skip,
                "executable not found".to_string(),
            ),
        }
        let sandbox = self.sandbox(target, executable.as_deref());
        self.network(target, executable.as_deref(), sandbox.as_ref());
    }

    fn claude_code_login(&self, target: &Target, executable: &Path) -> Result<String, String> {
        let argv = vec![
            executable.to_string_lossy().into_owned(),
            "auth".to_string(),
            "status".to_string(),
        ];
        let finished = self.execute(
            target.runtime,
            Check::Login,
            Run {
                argv: &argv,
                env: &[],
                session: &target.session,
            },
            |_| false,
        )?;
        let parsed: Option<Value> = serde_json::from_str(&finished.stdout.join("\n")).ok();
        match parsed.filter(|value| value["loggedIn"].is_boolean()) {
            Some(value) if value["loggedIn"] == true => {
                let token = target
                    .session
                    .env
                    .get(std::ffi::OsStr::new(TOKEN_VARIABLE))
                    .is_some_and(|value| !value.is_empty());
                Ok(if token {
                    TOKEN_VARIABLE.to_string()
                } else {
                    value["authMethod"].as_str().unwrap_or_default().to_string()
                })
            }
            Some(_) => Err(NOT_LOGGED_IN.to_string()),
            None => Err(format!("claude auth status failed: {}", finished.message())),
        }
    }

    fn codex_login(&self, target: &Target, executable: &Path) -> Result<String, String> {
        let login = codex::login_file(&target.session, &self.cwd);
        codex::check_login(&target.session, &login)?;
        let argv = vec![
            executable.to_string_lossy().into_owned(),
            "login".to_string(),
            "status".to_string(),
        ];
        let finished = self.execute(
            target.runtime,
            Check::Login,
            Run {
                argv: &argv,
                env: &self.codex_env(),
                session: &target.session,
            },
            |_| false,
        )?;
        if finished.exit_code == Some(0) {
            Ok(finished.output())
        } else {
            Err(format!("codex login status failed: {}", finished.output()))
        }
    }

    fn codex_env(&self) -> Vec<(OsString, OsString)> {
        vec![(
            OsString::from(codex::HOME_VARIABLE),
            self.codex_home.clone().into_os_string(),
        )]
    }

    fn pi_expected_model(&self, target: &Target) -> Result<(Model, &'static str), String> {
        if let Some(model) = &self.args.model {
            return Ok((pi::parse_model(model)?, "from --model"));
        }
        let settings = self.pi_settings_file(target);
        let model = pi::default_model(&settings).ok_or_else(|| {
            format!(
                "no --model given and {} has no defaultProvider and defaultModel",
                settings.display()
            )
        })?;
        Ok((model, "from pi settings"))
    }

    fn pi_settings_file(&self, target: &Target) -> PathBuf {
        pi::user_state_dir(&target.session, &self.cwd)
            .unwrap_or_else(|| home_placeholder(pi::STATE_HOME_SUBDIR))
            .join(pi::SETTINGS_FILE)
    }

    fn pi_login(&self, target: &Target, executable: &Path) -> Result<String, String> {
        let (expected, source) = self.pi_expected_model(target)?;
        let invocation = Invocation {
            runtime: Runtime::Pi,
            args: self.run_args(),
            cwd: self.cwd.clone(),
            prompt: PROMPT.to_string(),
            format: Format::Jsonl,
            sandbox: Sandbox {
                mode: SandboxMode::Off,
                wrapper: Wrapper::None,
                reason: String::new(),
                description: String::new(),
            },
            tempdir: self.tempdir.clone(),
            session: target.session.clone(),
            allow_hosts: Vec::new(),
            proxy: None,
        };
        let launch = Pi::new().launch(executable, &invocation)?;
        let finished = self.execute(
            target.runtime,
            Check::Login,
            Run {
                argv: &launch.argv,
                env: &launch.env,
                session: &target.session,
            },
            |line| assistant_start(line).is_some(),
        )?;
        let chosen = finished
            .stdout
            .iter()
            .find_map(|line| assistant_start(line));
        match chosen {
            Some(actual) if actual == expected => match unknown_model_warning(&finished) {
                Some(warning) => Err(format!(
                    "pi does not know {}/{} ({source}): {warning}",
                    actual.provider, actual.model
                )),
                None => Ok(format!("{}/{} ({source})", actual.provider, actual.model)),
            },
            Some(actual) => Err(format!(
                "pi uses {}/{}, expected {}/{} ({source}). Check the API key for {}",
                actual.provider, actual.model, expected.provider, expected.model, expected.provider
            )),
            None => Err(finished
                .stderr_line()
                .map(str::to_string)
                .unwrap_or_else(|| format!("pi {} before choosing a model", finished.exit_text))),
        }
    }

    fn run_args(&self) -> RunArgs {
        RunArgs {
            cwd: None,
            prompt: Some(PROMPT.to_string()),
            prompt_file: None,
            model: self.args.model.clone(),
            effort: None,
            max_turns: None,
            timeout: None,
            sandbox: Some(SandboxMode::Off),
            network: NetworkMode::None,
            allow_host: Vec::new(),
            path: self.args.path.clone(),
            env: self.args.env.clone(),
            env_file: self.args.env_file.clone(),
            no_subagents: false,
            format: Some(Format::Jsonl),
            raw: None,
            debug: self.args.debug,
            dry_run: false,
            runtime_args: Vec::new(),
        }
    }

    fn sandbox(&self, target: &Target, executable: Option<&Path>) -> Option<Sandbox> {
        let runtime = target.runtime;
        if target.mode == SandboxMode::Off {
            self.record(
                runtime,
                Check::Sandbox,
                Status::Skip,
                "--sandbox off".to_string(),
            );
            return None;
        }
        let sandbox =
            match sandbox::check(target.mode, runtime, &target.session, &self.cwd, &|pid| {
                self.caller.signals.checking(pid)
            }) {
                Ok(sandbox) if sandbox.runs() => sandbox,
                Ok(sandbox) => {
                    self.record(runtime, Check::Sandbox, Status::Skip, sandbox.reason);
                    return None;
                }
                Err(detail) => {
                    let detail = detail
                        .strip_prefix(UNAVAILABLE_PREFIX)
                        .unwrap_or(&detail)
                        .to_string();
                    self.record(runtime, Check::Sandbox, Status::Fail, detail);
                    return None;
                }
            };
        let result = match (runtime, executable) {
            (Runtime::Codex, None) => Err("executable not found".to_string()),
            _ => self.sandbox_test(target, executable, &sandbox),
        };
        self.record_result(runtime, Check::Sandbox, result);
        Some(sandbox)
    }

    fn check_dirs(&self, runtime: Runtime, check: Check) -> Result<Dirs, String> {
        let base = self
            .tempdir
            .join(format!("{}-{}", runtime.name(), check.name()));
        let dirs = Dirs {
            work: base.join("work"),
            tmp: base.join("tmp"),
            base,
        };
        for dir in [&dirs.work, &dirs.tmp] {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        }
        Ok(dirs)
    }

    fn wrap(
        &self,
        target: &Target,
        sandbox: &Sandbox,
        dirs: &Dirs,
        network: NetworkMode,
        proxy: Option<&ProxyEndpoint>,
    ) -> Result<Wrap, String> {
        let state = match target.runtime {
            Runtime::Pi => pi::user_state_dir(&target.session, &self.cwd)
                .map(|dir| pi::prepare_state(&dir))
                .transpose()?,
            _ => None,
        };
        match &sandbox.wrapper {
            Wrapper::Bubblewrap { .. } => Ok(Wrap::Bubblewrap(sandbox::wrap(
                sandbox,
                &dirs.work,
                &dirs.tmp,
                state.as_ref(),
                proxy,
                false,
            )?)),
            Wrapper::Seatbelt => Ok(Wrap::Seatbelt(sandbox::wrap(
                sandbox,
                &dirs.work,
                &dirs.tmp,
                state.as_ref(),
                proxy,
                false,
            )?)),
            Wrapper::Codex => {
                let tmp = std::fs::canonicalize(&dirs.tmp)
                    .map_err(|error| format!("cannot resolve {}: {error}", dirs.tmp.display()))?;
                let mut prefix = vec![
                    "sandbox".to_string(),
                    "-P".to_string(),
                    codex::PROFILE.to_string(),
                ];
                prefix.extend(codex::config(codex::filesystem_setting(&tmp)));
                prefix.extend(codex::network_settings(network, &self.allow_hosts));
                prefix.push("-C".to_string());
                prefix.push(dirs.work.to_string_lossy().into_owned());
                prefix.push("--".to_string());
                Ok(Wrap::Codex { prefix })
            }
            Wrapper::None => Err("sandbox not running".to_string()),
        }
    }

    fn sandbox_test(
        &self,
        target: &Target,
        executable: Option<&Path>,
        sandbox: &Sandbox,
    ) -> Result<String, String> {
        let runtime = target.runtime;
        let dirs = self.check_dirs(runtime, Check::Sandbox)?;
        let wrap = self.wrap(target, sandbox, &dirs, NetworkMode::None, None)?;
        let script = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            SANDBOX_SCRIPT.to_string(),
            "sh".to_string(),
            dirs.work.to_string_lossy().into_owned(),
            dirs.base.to_string_lossy().into_owned(),
        ];
        let finished = self.execute(
            runtime,
            Check::Sandbox,
            Run {
                argv: &wrap.argv(executable, &script),
                env: &wrap.env(self),
                session: &target.session,
            },
            |_| false,
        )?;
        if !finished.stdout.iter().any(|line| line == "done") {
            return Err(bwrap_hint(
                format!("the command did not run: {}", finished.message()),
                &finished,
            ));
        }
        if !dirs.work.join(INSIDE_FILE).exists() {
            return Err("the command could not write the working directory".to_string());
        }
        if dirs.base.join(OUTSIDE_FILE).exists() {
            return Err("a write outside the allowed directories succeeded".to_string());
        }
        let mut detail = format!("{}: wrote inside, blocked outside", wrap.name());
        if runtime == Runtime::Pi {
            match executable {
                Some(pi) => {
                    let version = vec![pi.to_string_lossy().into_owned(), "--version".to_string()];
                    let finished = self.execute(
                        runtime,
                        Check::Sandbox,
                        Run {
                            argv: &wrap.argv(executable, &version),
                            env: &wrap.env(self),
                            session: &target.session,
                        },
                        |_| false,
                    )?;
                    if finished.exit_code != Some(0) {
                        return Err(bwrap_hint(
                            format!("pi did not start in the sandbox: {}", finished.message()),
                            &finished,
                        ));
                    }
                    detail.push_str(", pi started");
                }
                None => detail.push_str(", pi not started (executable not found)"),
            }
        }
        Ok(detail)
    }

    fn network(&self, target: &Target, executable: Option<&Path>, sandbox: Option<&Sandbox>) {
        let runtime = target.runtime;
        if self.args.network == NetworkMode::None {
            self.record(
                runtime,
                Check::Network,
                Status::Skip,
                "--network none".to_string(),
            );
            return;
        }
        let Some(sandbox) = sandbox else {
            self.record(
                runtime,
                Check::Network,
                Status::Skip,
                "sandbox not running".to_string(),
            );
            return;
        };
        let result = match (runtime, executable) {
            (Runtime::Codex, None) => Err("executable not found".to_string()),
            _ => self.network_test(target, executable, sandbox),
        };
        self.record_result(runtime, Check::Network, result);
    }

    fn allowed_target(&self, target: &Target) -> (String, u16) {
        if self.args.network == NetworkMode::Custom {
            let rule = &self.allow_hosts[0];
            let host = if rule.wildcard {
                format!("www.{}", rule.host)
            } else {
                rule.host.clone()
            };
            return (host, rule.port.unwrap_or(SERVICE_PORT));
        }
        let host = match target.runtime {
            Runtime::ClaudeCode => CLAUDE_CODE_SERVICE_HOST,
            Runtime::Codex => CODEX_SERVICE_HOST,
            Runtime::Pi => self
                .pi_provider(target)
                .as_deref()
                .and_then(pi::service_host)
                .unwrap_or(PI_FALLBACK_SERVICE_HOST),
        };
        (host.to_string(), SERVICE_PORT)
    }

    fn pi_provider(&self, target: &Target) -> Option<String> {
        let expected = match &self.args.model {
            Some(model) => Some(pi::parse_model(model).ok()?),
            None => None,
        };
        pi::service_provider(
            expected.as_ref(),
            pi::user_state_dir(&target.session, &self.cwd).as_deref(),
        )
    }

    fn network_test(
        &self,
        target: &Target,
        executable: Option<&Path>,
        sandbox: &Sandbox,
    ) -> Result<String, String> {
        let runtime = target.runtime;
        let mode = self.args.network;
        let dirs = self.check_dirs(runtime, Check::Network)?;
        let allowed = self.allowed_target(target);
        let denied = match (mode, runtime) {
            (NetworkMode::Custom, _) => {
                Some((DENIED_HOST, SERVICE_PORT, NetworkReason::NotAllowed))
            }
            (_, Runtime::Codex) => None,
            _ => Some((PRIVATE_HOST, PRIVATE_PORT, NetworkReason::PrivateAddress)),
        };
        let records: Arc<Mutex<Vec<Network>>> = Arc::default();
        let mut proxy = None;
        let mut env = Vec::new();
        let mut endpoint = None;
        if network::proxy_needed(runtime, mode, sandbox.runs()) {
            let mut bound = FilterProxy::bind(&self.tempdir)?;
            let service_hosts = match runtime {
                Runtime::Pi => self
                    .pi_provider(target)
                    .as_deref()
                    .and_then(pi::service_host)
                    .map(|host| vec![host.to_string()])
                    .unwrap_or_default(),
                Runtime::Codex => codex::SERVICE_HOSTS
                    .iter()
                    .map(|host| host.to_string())
                    .collect(),
                Runtime::ClaudeCode => Vec::new(),
            };
            let seen = Arc::clone(&records);
            bound.serve_session(
                Policy {
                    mode,
                    rules: self.allow_hosts.clone(),
                    service_hosts,
                },
                &target.session.env,
                move |network| {
                    if let Ok(mut seen) = seen.lock() {
                        seen.push(network);
                    }
                },
            );
            env.extend(proxy_environment(&bound.endpoint().port_text()));
            endpoint = Some(bound.endpoint());
            proxy = Some(bound);
        }
        let wrap = self.wrap(target, sandbox, &dirs, mode, endpoint.as_ref())?;
        env.extend(wrap.env(self));
        let connect = |host: &str, port: u16| {
            let argv = vec![
                self.self_exe.to_string_lossy().into_owned(),
                "doctor-connect".to_string(),
                host.to_string(),
                port.to_string(),
            ];
            self.execute(
                runtime,
                Check::Network,
                Run {
                    argv: &wrap.argv(executable, &argv),
                    env: &env,
                    session: &target.session,
                },
                |_| false,
            )
        };
        let result = self.judge(&allowed, denied, &connect, &records);
        drop(proxy);
        result.map_err(|detail| self.fake_ip_hint(target, &allowed.0, detail))
    }

    fn judge(
        &self,
        allowed: &(String, u16),
        denied: Option<(&str, u16, NetworkReason)>,
        connect: &dyn Fn(&str, u16) -> Result<Finished, String>,
        records: &Mutex<Vec<Network>>,
    ) -> Result<String, String> {
        let (allowed_host, allowed_port) = allowed;
        let first = connect(allowed_host, *allowed_port)?;
        if first.exit_code != Some(0) {
            return Err(format!(
                "{allowed_host}:{allowed_port} not reachable: {}",
                first.output()
            ));
        }
        let mut detail = format!("{allowed_host}:{allowed_port} reachable");
        let Some((host, port, reason)) = denied else {
            return Ok(detail);
        };
        let second = connect(host, port)?;
        if second.exit_code == Some(0) {
            return Err(format!(
                "{host}:{port} was not refused: {}",
                second.output()
            ));
        }
        let seen = records
            .lock()
            .map(|records| {
                records
                    .iter()
                    .find(|record| record.host == host && record.port == port)
                    .cloned()
            })
            .unwrap_or_default();
        match seen {
            Some(record) if !record.allowed && record.reason == Some(reason) => {
                detail.push_str(&format!(", {host}:{port} refused ({})", reason.name()));
                Ok(detail)
            }
            Some(record) if !record.allowed => Err(format!(
                "{host}:{port} refused with reason {}, expected {}",
                record.reason.map(NetworkReason::name).unwrap_or("none"),
                reason.name()
            )),
            Some(_) => Err(format!(
                "{host}:{port} was allowed by the filter proxy, but the connection failed: {}",
                second.output()
            )),
            None if second.output().contains(CODEX_PROXY_NOT_ALLOWED) => {
                detail.push_str(&format!(", {host}:{port} refused (by the codex proxy)"));
                Ok(detail)
            }
            None => Err(format!(
                "{host}:{port} refused, but the filter proxy did not see the request: {}",
                second.output()
            )),
        }
    }

    fn fake_ip_hint(&self, target: &Target, allowed_host: &str, detail: String) -> String {
        if target.runtime != Runtime::Codex || self.args.network != NetworkMode::Custom {
            return detail;
        }
        for host in [allowed_host, CODEX_SERVICE_HOST] {
            if let Some(address) = fake_ip(host) {
                return format!("{detail}. {host} resolves to {address} (fake-ip). {FAKE_IP_HINT}");
            }
        }
        detail
    }
}

fn unknown_model_warning(finished: &Finished) -> Option<&str> {
    finished.stderr.lines().map(str::trim).find(|line| {
        line.starts_with(PI_UNKNOWN_MODEL_PREFIX) && line.contains(PI_UNKNOWN_MODEL_TEXT)
    })
}

fn remove_login_link(tempdir: &Path) {
    let link = tempdir.join(CODEX_HOME_DIR).join(codex::LOGIN_FILE);
    if std::fs::symlink_metadata(&link).is_ok_and(|metadata| metadata.is_symlink()) {
        let _ = std::fs::remove_file(&link);
    }
}

fn bwrap_hint(detail: String, finished: &Finished) -> String {
    match finished.stderr_line() {
        Some(line) if line.starts_with(BWRAP_PREFIX) => format!("{detail}. {CANNOT_START_HINT}"),
        _ => detail,
    }
}

fn assistant_start(line: &str) -> Option<Model> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value["type"] != "message_start" {
        return None;
    }
    pi::assistant_model(&value["message"])
}

fn fake_ip(host: &str) -> Option<Ipv4Addr> {
    let (network, bits) = FAKE_IP_RANGE;
    (host, SERVICE_PORT)
        .to_socket_addrs()
        .ok()?
        .find_map(|address| match address {
            SocketAddr::V4(v4) if in_cidr(*v4.ip(), network, bits) => Some(*v4.ip()),
            _ => None,
        })
}

struct Dirs {
    base: PathBuf,
    work: PathBuf,
    tmp: PathBuf,
}

enum Wrap {
    Bubblewrap(Wrapped),
    Seatbelt(Wrapped),
    Codex { prefix: Vec<String> },
}

impl Wrap {
    fn name(&self) -> &'static str {
        match self {
            Wrap::Bubblewrap(_) => "bubblewrap",
            Wrap::Seatbelt(_) => "seatbelt",
            Wrap::Codex { .. } => "codex",
        }
    }

    fn argv(&self, executable: Option<&Path>, inner: &[String]) -> Vec<String> {
        match self {
            Wrap::Bubblewrap(wrapped) | Wrap::Seatbelt(wrapped) => wrapped.argv(inner),
            Wrap::Codex { prefix } => {
                let mut argv = vec![
                    executable
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|| Runtime::Codex.executable().to_string()),
                ];
                argv.extend(prefix.iter().cloned());
                argv.extend(inner.iter().cloned());
                argv
            }
        }
    }

    fn env(&self, doctor: &Doctor) -> Vec<(OsString, OsString)> {
        match self {
            Wrap::Codex { .. } => doctor.codex_env(),
            _ => Vec::new(),
        }
    }
}

pub(crate) fn connect(caller: &Caller, args: &ConnectArgs) -> u8 {
    let proxy = CONNECT_PROXY_VARIABLES
        .iter()
        .find_map(|name| caller.var(name).filter(|value| !value.is_empty()));
    let (line, code) = match proxy {
        Some(proxy) => match ProxyAddress::parse(&proxy.to_string_lossy()) {
            Some(address) => connect_via_proxy(&address, &args.host, args.port),
            None => (
                "failed: the proxy variable does not hold an http:// address".to_string(),
                1,
            ),
        },
        None => connect_directly(&args.host, args.port),
    };
    caller.print_lines(&[line]);
    code
}

fn connect_via_proxy(proxy: &ProxyAddress, host: &str, port: u16) -> (String, u8) {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let stream = loop {
        match connect_once(&proxy.host, proxy.port, deadline) {
            Ok(stream) => break stream,
            Err(error)
                if error.kind() == ErrorKind::ConnectionRefused && Instant::now() < deadline =>
            {
                thread::sleep(CONNECT_RETRY);
            }
            Err(error) => return (format!("failed: {error}"), 1),
        }
    };
    match tunnel(stream, host, port, deadline) {
        Ok(status_line) => {
            let code = status_line
                .split_whitespace()
                .nth(1)
                .and_then(|code| code.parse::<u16>().ok());
            match code {
                Some(code) if (200..300).contains(&code) => {
                    (format!("connected via proxy ({code})"), 0)
                }
                _ => (format!("proxy replied {status_line}"), 1),
            }
        }
        Err(error) => (format!("failed: {error}"), 1),
    }
}

fn tunnel(
    mut stream: TcpStream,
    host: &str,
    port: u16,
    deadline: Instant,
) -> std::io::Result<String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    stream.set_read_timeout(Some(remaining.max(POLL)))?;
    stream.set_write_timeout(Some(remaining.max(POLL)))?;
    stream.write_all(
        format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n").as_bytes(),
    )?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    if status_line.is_empty() {
        return Err(std::io::Error::new(
            ErrorKind::UnexpectedEof,
            "the proxy closed the connection without a reply",
        ));
    }
    let mut reply = status_line.trim_end().to_string();
    let mut header = String::new();
    while reader.read_line(&mut header)? > 0 && !header.trim().is_empty() {
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case(PROXY_ERROR_HEADER)
        {
            reply.push_str(&format!(" ({PROXY_ERROR_HEADER}: {})", value.trim()));
        }
        header.clear();
    }
    Ok(reply)
}

fn connect_directly(host: &str, port: u16) -> (String, u8) {
    match connect_once(host, port, Instant::now() + CONNECT_TIMEOUT) {
        Ok(_) => ("connected".to_string(), 0),
        Err(error) => (format!("failed: {error}"), 1),
    }
}

fn connect_once(host: &str, port: u16, deadline: Instant) -> std::io::Result<TcpStream> {
    let address = (host, port)
        .to_socket_addrs()?
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| {
            std::io::Error::new(ErrorKind::NotFound, format!("{host} has no IPv4 address"))
        })?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    TcpStream::connect_timeout(&address, remaining.max(POLL))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_aligned_in_columns() {
        let item = Item {
            runtime: Runtime::Pi,
            check: Check::Login,
            status: Status::Ok,
            detail: "deepseek/deepseek-flash (from pi settings)".to_string(),
        };
        assert_eq!(
            item.line(),
            "[ok]   pi           login       deepseek/deepseek-flash (from pi settings)"
        );
        let item = Item {
            runtime: Runtime::ClaudeCode,
            check: Check::Executable,
            status: Status::Fail,
            detail: "claude not found in PATH".to_string(),
        };
        assert_eq!(
            item.line(),
            "[fail] claude-code  executable  claude not found in PATH"
        );
        assert_eq!(
            serde_json::to_string(&item).unwrap(),
            r#"{"runtime":"claude-code","check":"executable","status":"fail","detail":"claude not found in PATH"}"#
        );
    }

    #[test]
    fn doctor_commands_are_selected_from_the_first_argument() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(selected(&args(&["agentrun", "doctor"])));
        assert!(selected(&args(&["agentrun", "doctor-connect", "h", "1"])));
        assert!(!selected(&args(&["agentrun", "pi", "doctor"])));
        assert!(!selected(&args(&["agentrun"])));
    }

    #[test]
    fn assistant_message_start_gives_the_model() {
        let line = r#"{"type":"message_start","message":{"role":"assistant","provider":"deepseek","model":"deepseek-flash"}}"#;
        assert_eq!(
            assistant_start(line),
            Some(Model {
                provider: "deepseek".to_string(),
                model: "deepseek-flash".to_string(),
            })
        );
        assert_eq!(
            assistant_start(r#"{"type":"message_start","message":{"role":"system"}}"#),
            None
        );
        assert_eq!(assistant_start("not json"), None);
    }

    #[test]
    fn fake_ip_only_matches_the_reserved_range() {
        assert_eq!(
            fake_ip("198.19.255.1"),
            Some(Ipv4Addr::new(198, 19, 255, 1))
        );
        assert_eq!(fake_ip("198.18.0.1"), Some(Ipv4Addr::new(198, 18, 0, 1)));
        assert_eq!(fake_ip("198.20.0.1"), None);
        assert_eq!(fake_ip("127.0.0.1"), None);
    }

    #[test]
    fn proxy_reply_keeps_the_codex_proxy_error_header() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 256];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nX-Proxy-Error: blocked-by-allowlist\r\n\r\n")
                .unwrap();
        });
        let proxy = ProxyAddress {
            host: "127.0.0.1".to_string(),
            port,
            authorization: None,
        };
        let (line, code) = connect_via_proxy(&proxy, DENIED_HOST, SERVICE_PORT);
        server.join().unwrap();
        assert_eq!(code, 1);
        assert_eq!(
            line,
            format!("proxy replied HTTP/1.1 403 Forbidden ({CODEX_PROXY_NOT_ALLOWED})")
        );
    }
}
