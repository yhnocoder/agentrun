mod bubblewrap;
mod seatbelt;

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::{Runtime, SandboxMode};
use crate::network::ProxyEndpoint;
use crate::output::SandboxKind;
use crate::run::{Signals, wait_for_exit};
use crate::session::Session;
#[cfg(target_os = "linux")]
use bubblewrap::probe;
use seatbelt::SANDBOX_EXEC_PREFIX;
#[cfg(target_os = "macos")]
use seatbelt::probe;

pub use bubblewrap::{ProxyForward, wrap_bwrap};
pub use seatbelt::{seatbelt_profile, wrap_seatbelt, write_seatbelt_profile};

pub(crate) use bubblewrap::{BWRAP_PREFIX, CANNOT_START_HINT};
use seatbelt::SEATBELT_FILE;

const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const CHECK_POLL: Duration = Duration::from_millis(10);
pub(crate) const UNAVAILABLE_PREFIX: &str = "sandbox is not available: ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wrapper {
    Bubblewrap { bwrap: PathBuf, socat: PathBuf },
    Seatbelt,
    Codex,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sandbox {
    pub mode: SandboxMode,
    pub wrapper: Wrapper,
    pub reason: String,
    pub description: String,
}

impl Sandbox {
    pub fn kind(&self) -> SandboxKind {
        match self.wrapper {
            Wrapper::Bubblewrap { .. } => SandboxKind::Bubblewrap,
            Wrapper::Seatbelt => SandboxKind::Seatbelt,
            Wrapper::Codex => SandboxKind::Codex,
            Wrapper::None => SandboxKind::None,
        }
    }

    pub fn runs(&self) -> bool {
        self.wrapper != Wrapper::None
    }
}

struct Available {
    wrapper: Wrapper,
    description: String,
}

struct Unavailable {
    reason: String,
    hint: &'static str,
}

pub(crate) fn resolve_mode(
    option: Option<SandboxMode>,
    variable: Option<&OsStr>,
) -> Result<SandboxMode, String> {
    if let Some(mode) = option {
        return Ok(mode);
    }
    match variable.and_then(|value| value.to_str()) {
        None | Some("") => Ok(SandboxMode::On),
        Some("on") => Ok(SandboxMode::On),
        Some("relax") => Ok(SandboxMode::Relax),
        Some("off") => Ok(SandboxMode::Off),
        Some(_) => Err(format!(
            "invalid AGENTRUN_SANDBOX value '{}': expected on, relax or off",
            variable.unwrap_or_default().to_string_lossy()
        )),
    }
}

pub(crate) fn check(
    mode: SandboxMode,
    runtime: Runtime,
    session: &Session,
    cwd: &Path,
    signals: &Signals,
) -> Result<Sandbox, String> {
    if mode == SandboxMode::Off {
        return Ok(Sandbox {
            mode,
            wrapper: Wrapper::None,
            reason: String::new(),
            description: "none (--sandbox off)".to_string(),
        });
    }
    match probe(runtime, session, cwd, signals) {
        Ok(available) => Ok(Sandbox {
            mode,
            wrapper: available.wrapper,
            reason: String::new(),
            description: available.description,
        }),
        Err(unavailable) if mode == SandboxMode::On => Err(format!(
            "{UNAVAILABLE_PREFIX}{}. {}",
            unavailable.reason, unavailable.hint
        )),
        Err(unavailable) => Ok(Sandbox {
            mode,
            wrapper: Wrapper::None,
            description: format!("none (--sandbox relax: {})", unavailable.reason),
            reason: unavailable.reason,
        }),
    }
}

fn start_check(
    program: &Path,
    args: &[&str],
    prefix: &str,
    session: &Session,
    cwd: &Path,
    signals: &Signals,
) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(&session.env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let pid = child.id() as i32;
    signals.checking(Some(pid));
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let exited = exits_within_check_timeout(pid);
    if !exited {
        let _ = child.kill();
    }
    signals.checking(None);
    let status = child.wait().map_err(|error| error.to_string())?;
    if !exited {
        return Err(format!(
            "timed out after {} seconds",
            CHECK_TIMEOUT.as_secs()
        ));
    }
    if status.success() {
        return Ok(());
    }
    let bytes = reader.join().unwrap_or_default();
    Err(failure_reason(
        status.code(),
        &String::from_utf8_lossy(&bytes),
        prefix,
    ))
}

fn exits_within_check_timeout(pid: i32) -> bool {
    let deadline = Instant::now() + CHECK_TIMEOUT;
    while !wait_for_exit(pid, false) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(CHECK_POLL);
    }
    true
}

fn failure_reason(exit_code: Option<i32>, stderr: &str, prefix: &str) -> String {
    if let Some(line) = stderr.lines().map(str::trim).find(|line| !line.is_empty()) {
        return line.strip_prefix(prefix).unwrap_or(line).to_string();
    }
    exit_text(exit_code)
}

pub(crate) fn exit_text(exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => format!("exited with code {code}"),
        None => "terminated by a signal".to_string(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiState {
    pub dir: PathBuf,
    pub writable: Vec<PathBuf>,
    pub login_target: Option<PathBuf>,
    pub readonly: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wrapped {
    pub kind: SandboxKind,
    pub prefix: Vec<String>,
}

impl Wrapped {
    pub fn argv(&self, command: &[String]) -> Vec<String> {
        let mut argv = self.prefix.clone();
        argv.extend(command.iter().cloned());
        argv
    }
}

pub fn wrap(
    sandbox: &Sandbox,
    cwd: &Path,
    tempdir: &Path,
    state: Option<&PiState>,
    proxy: Option<&ProxyEndpoint>,
    dry_run: bool,
) -> Result<Wrapped, String> {
    let port = proxy.map(ProxyEndpoint::port_text);
    match &sandbox.wrapper {
        Wrapper::Bubblewrap { bwrap, socat } => {
            let forward = proxy
                .zip(port.as_deref())
                .map(|(endpoint, port)| ProxyForward {
                    socat,
                    port,
                    socket: &endpoint.socket,
                });
            Ok(Wrapped {
                kind: SandboxKind::Bubblewrap,
                prefix: wrap_bwrap(bwrap, cwd, tempdir, state, forward.as_ref(), &[]),
            })
        }
        Wrapper::Seatbelt => {
            if !dry_run {
                write_seatbelt_profile(cwd, tempdir, state, port.as_deref()).map_err(|error| {
                    format!(
                        "cannot write the sandbox profile {}: {error}",
                        tempdir.join(SEATBELT_FILE).display()
                    )
                })?;
            }
            Ok(Wrapped {
                kind: SandboxKind::Seatbelt,
                prefix: wrap_seatbelt(tempdir, &[]),
            })
        }
        Wrapper::Codex | Wrapper::None => Ok(Wrapped {
            kind: SandboxKind::None,
            prefix: Vec::new(),
        }),
    }
}

pub fn wrapper_failure(
    kind: SandboxKind,
    exit_code: Option<i32>,
    stderr_tail: &str,
) -> Option<String> {
    let prefix = match kind {
        SandboxKind::Bubblewrap => BWRAP_PREFIX,
        SandboxKind::Seatbelt => SANDBOX_EXEC_PREFIX,
        SandboxKind::Codex | SandboxKind::None => return None,
    };
    if exit_code == Some(0) {
        return None;
    }
    stderr_tail
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .map(start_failure)
}

pub fn start_failure(reason: &str) -> String {
    format!("sandbox failed to start: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    fn pairs(list: &[(&str, &str)]) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        list.iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect()
    }

    #[test]
    fn option_wins_over_variable() {
        assert_eq!(
            resolve_mode(Some(SandboxMode::On), Some(OsStr::new("off"))),
            Ok(SandboxMode::On)
        );
        assert_eq!(
            resolve_mode(Some(SandboxMode::Relax), Some(OsStr::new("maybe"))),
            Ok(SandboxMode::Relax)
        );
    }

    #[test]
    fn variable_values_and_default() {
        assert_eq!(
            resolve_mode(None, Some(OsStr::new("on"))),
            Ok(SandboxMode::On)
        );
        assert_eq!(
            resolve_mode(None, Some(OsStr::new("relax"))),
            Ok(SandboxMode::Relax)
        );
        assert_eq!(
            resolve_mode(None, Some(OsStr::new("off"))),
            Ok(SandboxMode::Off)
        );
        assert_eq!(
            resolve_mode(None, Some(OsStr::new(""))),
            Ok(SandboxMode::On)
        );
        assert_eq!(resolve_mode(None, None), Ok(SandboxMode::On));
    }

    #[test]
    fn invalid_variable_value_is_a_usage_error() {
        for value in ["maybe", "ON", "Off", " on"] {
            assert_eq!(
                resolve_mode(None, Some(OsStr::new(value))),
                Err(format!(
                    "invalid AGENTRUN_SANDBOX value '{value}': expected on, relax or off"
                ))
            );
        }
    }

    #[test]
    fn failure_reason_takes_first_line_without_prefix() {
        assert_eq!(
            failure_reason(
                Some(1),
                "\n  \nbwrap: No permissions to create new namespace\nbwrap: second\n",
                BWRAP_PREFIX
            ),
            "No permissions to create new namespace"
        );
        assert_eq!(
            failure_reason(
                Some(65),
                "sandbox-exec: sandbox_apply: Operation not permitted\n",
                SANDBOX_EXEC_PREFIX
            ),
            "sandbox_apply: Operation not permitted"
        );
        assert_eq!(
            failure_reason(Some(1), "bwrap: kept\n", SANDBOX_EXEC_PREFIX),
            "bwrap: kept"
        );
        assert_eq!(
            failure_reason(Some(1), "plain error\n", BWRAP_PREFIX),
            "plain error"
        );
        assert_eq!(
            failure_reason(Some(3), "", BWRAP_PREFIX),
            "exited with code 3"
        );
        assert_eq!(
            failure_reason(Some(3), " \n\t\n", SANDBOX_EXEC_PREFIX),
            "exited with code 3"
        );
        assert_eq!(
            failure_reason(None, "", SANDBOX_EXEC_PREFIX),
            "terminated by a signal"
        );
    }

    #[test]
    fn exit_text_names_the_code_or_the_signal() {
        assert_eq!(exit_text(Some(3)), "exited with code 3");
        assert_eq!(exit_text(None), "terminated by a signal");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_is_available_on_macos_and_codex_uses_its_own() {
        let signals = Signals::install();
        let session = Session::assemble(Runtime::Pi, &[], &[], &[], &[]);
        for runtime in [Runtime::Pi, Runtime::ClaudeCode] {
            let sandbox =
                check(SandboxMode::On, runtime, &session, Path::new("/"), &signals).unwrap();
            assert_eq!(sandbox.wrapper, Wrapper::Seatbelt);
            let description = &sandbox.description;
            let millis = description
                .strip_prefix("seatbelt (/usr/bin/sandbox-exec, check ")
                .and_then(|rest| rest.strip_suffix("ms)"))
                .unwrap_or_else(|| panic!("{description}"));
            assert!(millis.parse::<u128>().unwrap() < 5000, "{description}");
        }
        let codex = check(
            SandboxMode::On,
            Runtime::Codex,
            &session,
            Path::new("/"),
            &signals,
        )
        .unwrap();
        assert_eq!(codex.wrapper, Wrapper::Codex);
        assert_eq!(codex.description, "codex");
    }

    #[test]
    fn kind_follows_the_wrapper_and_only_none_does_not_run() {
        let cases = [
            (
                Wrapper::Bubblewrap {
                    bwrap: PathBuf::from("/usr/bin/bwrap"),
                    socat: PathBuf::from("/usr/bin/socat"),
                },
                SandboxKind::Bubblewrap,
                true,
            ),
            (Wrapper::Seatbelt, SandboxKind::Seatbelt, true),
            (Wrapper::Codex, SandboxKind::Codex, true),
            (Wrapper::None, SandboxKind::None, false),
        ];
        for (wrapper, kind, runs) in cases {
            let sandbox = Sandbox {
                mode: SandboxMode::On,
                wrapper,
                reason: String::new(),
                description: String::new(),
            };
            assert_eq!(sandbox.kind(), kind);
            assert_eq!(sandbox.runs(), runs);
        }
    }

    fn sandbox_with(wrapper: Wrapper) -> Sandbox {
        Sandbox {
            mode: SandboxMode::On,
            wrapper,
            reason: String::new(),
            description: String::new(),
        }
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn bubblewrap_wrap_forwards_the_proxy_through_socat() {
        let sandbox = sandbox_with(Wrapper::Bubblewrap {
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            socat: PathBuf::from("/usr/bin/socat"),
        });
        let cwd = Path::new("/nonexistent/work");
        let tempdir = Path::new("/nonexistent/tmp/agentrun-x");
        let socket = tempdir.join("proxy.sock");
        let command = strings(&["/usr/bin/pi", "-p"]);
        let endpoint = ProxyEndpoint {
            port: Some(41234),
            socket: socket.clone(),
        };
        let wrapped = wrap(&sandbox, cwd, tempdir, None, Some(&endpoint), false).unwrap();
        assert_eq!(wrapped.kind, SandboxKind::Bubblewrap);
        let forward = ProxyForward {
            socat: Path::new("/usr/bin/socat"),
            port: "41234",
            socket: &socket,
        };
        assert_eq!(
            wrapped.argv(&command),
            wrap_bwrap(
                Path::new("/usr/bin/bwrap"),
                cwd,
                tempdir,
                None,
                Some(&forward),
                &command
            )
        );
        let direct = wrap(&sandbox, cwd, tempdir, None, None, false).unwrap();
        assert_eq!(
            direct.argv(&command),
            wrap_bwrap(
                Path::new("/usr/bin/bwrap"),
                cwd,
                tempdir,
                None,
                None,
                &command
            )
        );
        assert!(!direct.prefix.contains(&"/bin/sh".to_string()));
        assert!(!direct.prefix.contains(&"-c".to_string()));
        let placeholder = ProxyEndpoint { port: None, socket };
        let dry = wrap(&sandbox, cwd, tempdir, None, Some(&placeholder), true).unwrap();
        assert!(dry.prefix.contains(&"<proxy port>".to_string()));
    }

    #[test]
    fn seatbelt_wrap_writes_the_profile_unless_dry_run() {
        let root = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(root.path()).unwrap();
        let work = real.join("work");
        let tempdir = real.join("session");
        std::fs::create_dir(&work).unwrap();
        std::fs::create_dir(&tempdir).unwrap();
        let sandbox = sandbox_with(Wrapper::Seatbelt);
        let profile = tempdir.join("seatbelt.sb");
        let prefix = strings(&["/usr/bin/sandbox-exec", "-f", profile.to_str().unwrap()]);
        let dry = wrap(&sandbox, &work, &tempdir, None, None, true).unwrap();
        assert_eq!(dry.kind, SandboxKind::Seatbelt);
        assert_eq!(dry.prefix, prefix);
        assert!(!profile.exists());
        let endpoint = ProxyEndpoint {
            port: Some(7),
            socket: tempdir.join("proxy.sock"),
        };
        let written = wrap(&sandbox, &work, &tempdir, None, Some(&endpoint), false).unwrap();
        assert_eq!(written.kind, SandboxKind::Seatbelt);
        assert_eq!(written.prefix, prefix);
        assert_eq!(
            std::fs::read_to_string(&profile).unwrap(),
            seatbelt_profile(&work, &tempdir, None, Some("7"))
        );
        let missing = real.join("missing");
        let detail = wrap(&sandbox, &work, &missing, None, None, false).unwrap_err();
        assert!(
            detail.starts_with(&format!(
                "cannot write the sandbox profile {}: ",
                missing.join("seatbelt.sb").display()
            )),
            "{detail}"
        );
    }

    #[test]
    fn codex_and_none_are_not_wrapped_by_agentrun() {
        let command = strings(&["/usr/bin/codex", "exec"]);
        for wrapper in [Wrapper::Codex, Wrapper::None] {
            let wrapped = wrap(
                &sandbox_with(wrapper),
                Path::new("/nonexistent/work"),
                Path::new("/nonexistent/tmp"),
                None,
                None,
                false,
            )
            .unwrap();
            assert_eq!(wrapped.kind, SandboxKind::None);
            assert_eq!(wrapped.argv(&command), command);
        }
    }

    #[test]
    fn off_mode_skips_the_check() {
        let signals = Signals::install();
        let session = Session::assemble(Runtime::Pi, &[], &[], &[], &[]);
        let sandbox = check(
            SandboxMode::Off,
            Runtime::Pi,
            &session,
            Path::new("/"),
            &signals,
        )
        .unwrap();
        assert_eq!(
            sandbox,
            Sandbox {
                mode: SandboxMode::Off,
                wrapper: Wrapper::None,
                reason: String::new(),
                description: "none (--sandbox off)".to_string(),
            }
        );
        assert!(!sandbox.runs());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_bwrap_rejects_on_and_passes_relax() {
        let signals = Signals::install();
        let session = Session::assemble(
            Runtime::ClaudeCode,
            &pairs(&[("PATH", "/nonexistent")]),
            &[],
            &[],
            &[],
        );
        assert_eq!(
            check(
                SandboxMode::On,
                Runtime::ClaudeCode,
                &session,
                Path::new("/"),
                &signals
            ),
            Err(
                "sandbox is not available: bwrap not found in PATH. Install bubblewrap and socat (for example: apt-get install bubblewrap socat, or dnf install bubblewrap socat), or use --sandbox relax or --sandbox off"
                    .to_string()
            )
        );
        let relaxed = check(
            SandboxMode::Relax,
            Runtime::ClaudeCode,
            &session,
            Path::new("/"),
            &signals,
        )
        .unwrap();
        assert_eq!(relaxed.wrapper, Wrapper::None);
        assert_eq!(relaxed.reason, "bwrap not found in PATH");
        assert_eq!(
            relaxed.description,
            "none (--sandbox relax: bwrap not found in PATH)"
        );
        assert_eq!(
            check(
                SandboxMode::On,
                Runtime::Codex,
                &session,
                Path::new("/"),
                &signals
            ),
            Err(
                "sandbox is not available: bwrap not found in PATH. Install bubblewrap (for example: apt-get install bubblewrap, or dnf install bubblewrap), or use --sandbox relax or --sandbox off"
                    .to_string()
            )
        );
    }

    #[test]
    fn start_failure_names_the_reason() {
        assert_eq!(start_failure("x"), "sandbox failed to start: x");
    }

    #[test]
    fn wrapper_failure_needs_nonzero_exit_and_bwrap_line() {
        let bwrap = SandboxKind::Bubblewrap;
        assert_eq!(
            wrapper_failure(
                bwrap,
                Some(1),
                "noise\nbwrap: Can't mkdir /x: Permission denied\n"
            ),
            Some("sandbox failed to start: Can't mkdir /x: Permission denied".to_string())
        );
        assert_eq!(
            wrapper_failure(bwrap, None, "bwrap: setting up uid map: Permission denied"),
            Some("sandbox failed to start: setting up uid map: Permission denied".to_string())
        );
        assert_eq!(wrapper_failure(bwrap, Some(0), "bwrap: ignored"), None);
        assert_eq!(
            wrapper_failure(bwrap, Some(1), "pi: something else\n"),
            None
        );
        assert_eq!(wrapper_failure(bwrap, Some(1), " bwrap: indented\n"), None);
        assert_eq!(
            wrapper_failure(bwrap, Some(1), "sandbox-exec: not this one\n"),
            None
        );
    }

    #[test]
    fn wrapper_failure_on_macos_looks_for_sandbox_exec_lines() {
        let seatbelt = SandboxKind::Seatbelt;
        assert_eq!(
            wrapper_failure(
                seatbelt,
                Some(65),
                "noise\nsandbox-exec: unbound variable: x\n"
            ),
            Some("sandbox failed to start: unbound variable: x".to_string())
        );
        assert_eq!(wrapper_failure(seatbelt, Some(0), "sandbox-exec: x"), None);
        assert_eq!(wrapper_failure(seatbelt, Some(1), "bwrap: x\n"), None);
        for kind in [SandboxKind::Codex, SandboxKind::None] {
            assert_eq!(wrapper_failure(kind, Some(1), "bwrap: x\n"), None);
            assert_eq!(wrapper_failure(kind, Some(1), "sandbox-exec: x\n"), None);
        }
    }
}
