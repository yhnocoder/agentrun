use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::{Runtime, SandboxMode};
use crate::event::SandboxKind;
use crate::session::Session;
use crate::signal::Signals;

pub const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const BWRAP_PREFIX: &str = "bwrap: ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sandbox {
    pub mode: SandboxMode,
    pub kind: SandboxKind,
    pub reason: String,
    pub bwrap: Option<PathBuf>,
    pub description: String,
}

impl Sandbox {
    pub fn runs(&self) -> bool {
        self.kind != SandboxKind::None
    }
}

struct Available {
    kind: SandboxKind,
    bwrap: Option<PathBuf>,
    description: String,
}

struct Unavailable {
    reason: String,
    hint: &'static str,
}

pub fn resolve_mode(
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

pub fn check(
    mode: SandboxMode,
    runtime: Runtime,
    session: &Session,
    cwd: &Path,
    signals: &Signals,
) -> Result<Sandbox, String> {
    if mode == SandboxMode::Off {
        return Ok(Sandbox {
            mode,
            kind: SandboxKind::None,
            reason: String::new(),
            bwrap: None,
            description: "none (--sandbox off)".to_string(),
        });
    }
    match probe(runtime, session, cwd, signals) {
        Ok(available) => Ok(Sandbox {
            mode,
            kind: available.kind,
            reason: String::new(),
            bwrap: available.bwrap,
            description: available.description,
        }),
        Err(unavailable) if mode == SandboxMode::On => Err(format!(
            "sandbox is not available: {}. {}",
            unavailable.reason, unavailable.hint
        )),
        Err(unavailable) => Ok(Sandbox {
            mode,
            kind: SandboxKind::None,
            description: format!("none (--sandbox relax: {})", unavailable.reason),
            reason: unavailable.reason,
            bwrap: None,
        }),
    }
}

#[cfg(target_os = "linux")]
use bubblewrap::probe;

#[cfg(not(target_os = "linux"))]
fn probe(
    _runtime: Runtime,
    _session: &Session,
    _cwd: &Path,
    _signals: &Signals,
) -> Result<Available, Unavailable> {
    Err(Unavailable {
        reason: "the macOS sandbox is not implemented in this build".to_string(),
        hint: "Use --sandbox relax or --sandbox off",
    })
}

#[cfg(target_os = "linux")]
mod bubblewrap {
    use std::io::Read;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{Available, CHECK_TIMEOUT, Unavailable, failure_reason};
    use crate::cli::Runtime;
    use crate::event::SandboxKind;
    use crate::session::{Session, find_executable};
    use crate::signal::Signals;

    const CHECK_POLL: Duration = Duration::from_millis(10);
    const INSTALL_HINT: &str = "Install bubblewrap and socat (for example: apt-get install bubblewrap socat, or dnf install bubblewrap socat), or use --sandbox relax or --sandbox off";
    const CANNOT_START_HINT: &str =
        "bwrap cannot create a sandbox here. In a docker container use --sandbox off";

    pub(super) fn probe(
        runtime: Runtime,
        session: &Session,
        cwd: &Path,
        signals: &Signals,
    ) -> Result<Available, Unavailable> {
        if runtime == Runtime::Codex {
            return Ok(Available {
                kind: SandboxKind::Codex,
                bwrap: None,
                description: "codex".to_string(),
            });
        }
        let missing = |name: &str| Unavailable {
            reason: format!("{name} not found in PATH"),
            hint: INSTALL_HINT,
        };
        let bwrap = find_executable("bwrap", &session.path, cwd).ok_or_else(|| missing("bwrap"))?;
        let socat = find_executable("socat", &session.path, cwd).ok_or_else(|| missing("socat"))?;
        let started = Instant::now();
        start_bwrap(&bwrap, session, cwd, signals).map_err(|reason| Unavailable {
            reason: format!("bwrap cannot start: {reason}"),
            hint: CANNOT_START_HINT,
        })?;
        Ok(Available {
            kind: SandboxKind::Bubblewrap,
            description: format!(
                "bubblewrap ({}, socat {}, check {}ms)",
                bwrap.display(),
                socat.display(),
                started.elapsed().as_millis()
            ),
            bwrap: Some(bwrap),
        })
    }

    fn start_bwrap(
        bwrap: &Path,
        session: &Session,
        cwd: &Path,
        signals: &Signals,
    ) -> Result<(), String> {
        let mut child = Command::new(bwrap)
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
            .current_dir(cwd)
            .env_clear()
            .envs(&session.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| error.to_string())?;
        signals.checking(Some(child.id() as i32));
        let outcome = wait_for_check(&mut child);
        signals.checking(None);
        outcome
    }

    fn wait_for_check(child: &mut Child) -> Result<(), String> {
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            bytes
        });
        let deadline = Instant::now() + CHECK_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => {
                    let bytes = reader.join().unwrap_or_default();
                    return Err(failure_reason(
                        status.code(),
                        &String::from_utf8_lossy(&bytes),
                    ));
                }
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "timed out after {} seconds",
                        CHECK_TIMEOUT.as_secs()
                    ));
                }
                Ok(None) => thread::sleep(CHECK_POLL),
                Err(error) => return Err(error.to_string()),
            }
        }
    }
}

pub fn failure_reason(exit_code: Option<i32>, stderr: &str) -> String {
    if let Some(line) = stderr.lines().map(str::trim).find(|line| !line.is_empty()) {
        return line.strip_prefix(BWRAP_PREFIX).unwrap_or(line).to_string();
    }
    match exit_code {
        Some(code) => format!("exited with code {code}"),
        None => "terminated by a signal".to_string(),
    }
}

pub fn wrap_pi(
    bwrap: &Path,
    cwd: &Path,
    tempdir: &Path,
    login_file: Option<&Path>,
    argv: &[String],
) -> Vec<String> {
    let text = |path: &Path| path.to_string_lossy().into_owned();
    let mut wrapped = vec![
        text(bwrap),
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
    ];
    let mut bind = |path: &Path| {
        wrapped.push("--bind".to_string());
        wrapped.push(text(path));
        wrapped.push(text(path));
    };
    bind(cwd);
    bind(tempdir);
    if let Some(login_file) = login_file {
        bind(login_file);
    }
    wrapped.extend(
        [
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--die-with-parent",
            "--unshare-net",
            "--",
        ]
        .map(str::to_string),
    );
    wrapped.extend(argv.iter().cloned());
    wrapped
}

pub fn wrapper_failure(exit_code: Option<i32>, stderr_tail: &str) -> Option<String> {
    if exit_code == Some(0) {
        return None;
    }
    stderr_tail
        .lines()
        .find_map(|line| line.strip_prefix(BWRAP_PREFIX))
        .map(|rest| format!("sandbox failed to start: {rest}"))
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

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| item.to_string()).collect()
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
                "\n  \nbwrap: No permissions to create new namespace\nbwrap: second\n"
            ),
            "No permissions to create new namespace"
        );
        assert_eq!(failure_reason(Some(1), "plain error\n"), "plain error");
        assert_eq!(failure_reason(Some(3), ""), "exited with code 3");
        assert_eq!(failure_reason(Some(3), " \n\t\n"), "exited with code 3");
        assert_eq!(failure_reason(None, ""), "terminated by a signal");
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
                kind: SandboxKind::None,
                reason: String::new(),
                bwrap: None,
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
        assert_eq!(relaxed.kind, SandboxKind::None);
        assert_eq!(relaxed.reason, "bwrap not found in PATH");
        assert_eq!(
            relaxed.description,
            "none (--sandbox relax: bwrap not found in PATH)"
        );
        let codex = check(
            SandboxMode::On,
            Runtime::Codex,
            &session,
            Path::new("/"),
            &signals,
        )
        .unwrap();
        assert_eq!(codex.kind, SandboxKind::Codex);
        assert_eq!(codex.description, "codex");
        assert!(codex.runs());
    }

    #[test]
    fn wrapped_argv_binds_cwd_tempdir_and_the_login_file_when_given() {
        let argv = strings(&["/usr/bin/pi", "-p", "hi"]);
        let with_login = wrap_pi(
            Path::new("/usr/bin/bwrap"),
            Path::new("/nonexistent/work"),
            Path::new("/nonexistent/tmp/agentrun-x"),
            Some(Path::new("/nonexistent/home/.pi/agent/auth.json")),
            &argv,
        );
        assert_eq!(
            with_login,
            strings(&[
                "/usr/bin/bwrap",
                "--ro-bind",
                "/",
                "/",
                "--bind",
                "/nonexistent/work",
                "/nonexistent/work",
                "--bind",
                "/nonexistent/tmp/agentrun-x",
                "/nonexistent/tmp/agentrun-x",
                "--bind",
                "/nonexistent/home/.pi/agent/auth.json",
                "/nonexistent/home/.pi/agent/auth.json",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--die-with-parent",
                "--unshare-net",
                "--",
                "/usr/bin/pi",
                "-p",
                "hi",
            ])
        );
        let without_login = wrap_pi(
            Path::new("/usr/bin/bwrap"),
            Path::new("/nonexistent/work"),
            Path::new("/nonexistent/tmp/agentrun-x"),
            None,
            &argv,
        );
        assert_eq!(
            without_login.iter().filter(|arg| *arg == "--bind").count(),
            2
        );
        assert_eq!(without_login.len(), with_login.len() - 3);
    }

    #[test]
    fn wrapper_failure_needs_nonzero_exit_and_bwrap_line() {
        assert_eq!(
            wrapper_failure(Some(1), "noise\nbwrap: Can't mkdir /x: Permission denied\n"),
            Some("sandbox failed to start: Can't mkdir /x: Permission denied".to_string())
        );
        assert_eq!(
            wrapper_failure(None, "bwrap: setting up uid map: Permission denied"),
            Some("sandbox failed to start: setting up uid map: Permission denied".to_string())
        );
        assert_eq!(wrapper_failure(Some(0), "bwrap: ignored"), None);
        assert_eq!(wrapper_failure(Some(1), "pi: something else\n"), None);
        assert_eq!(wrapper_failure(Some(1), " bwrap: indented\n"), None);
    }
}
