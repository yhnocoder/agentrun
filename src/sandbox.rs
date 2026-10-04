use std::ffi::OsStr;
use std::fs::DirBuilder;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::{Runtime, SandboxMode};
use crate::event::SandboxKind;
use crate::session::{Session, find_executable};
use crate::signal::Signals;

pub const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const CHECK_POLL: Duration = Duration::from_millis(10);
const BWRAP_PREFIX: &str = "bwrap: ";
#[cfg(target_os = "linux")]
const INSTALL_HINT: &str = "Install bubblewrap and socat (for example: apt-get install bubblewrap socat, or dnf install bubblewrap socat), or use --sandbox relax or --sandbox off";
#[cfg(target_os = "linux")]
const CANNOT_START_HINT: &str =
    "bwrap cannot create a sandbox here. In a docker container use --sandbox off";
#[cfg(not(target_os = "linux"))]
const NOT_IMPLEMENTED_HINT: &str = "Use --sandbox relax or --sandbox off";
const PI_STATE_DIR_VARIABLE: &str = "PI_CODING_AGENT_DIR";
const PI_STATE_HOME_SUBDIR: &str = ".pi/agent";

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
fn probe(
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

#[cfg(not(target_os = "linux"))]
fn probe(
    _runtime: Runtime,
    _session: &Session,
    _cwd: &Path,
    _signals: &Signals,
) -> Result<Available, Unavailable> {
    Err(Unavailable {
        reason: "not implemented in this build".to_string(),
        hint: NOT_IMPLEMENTED_HINT,
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

pub fn failure_reason(exit_code: Option<i32>, stderr: &str) -> String {
    if let Some(line) = stderr.lines().map(str::trim).find(|line| !line.is_empty()) {
        return line.strip_prefix(BWRAP_PREFIX).unwrap_or(line).to_string();
    }
    match exit_code {
        Some(code) => format!("exited with code {code}"),
        None => "terminated by a signal".to_string(),
    }
}

pub fn pi_state_dir(session: &Session, cwd: &Path) -> Result<PathBuf, String> {
    session
        .runtime_dir(cwd, PI_STATE_DIR_VARIABLE, PI_STATE_HOME_SUBDIR)
        .ok_or_else(|| {
            format!(
                "cannot create pi state directory $HOME/{PI_STATE_HOME_SUBDIR}: HOME is not set"
            )
        })
}

pub fn create_pi_state_dir(dir: &Path) -> Result<(), String> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|error| {
            format!(
                "cannot create pi state directory {}: {error}",
                dir.display()
            )
        })
}

pub fn wrap_pi(
    bwrap: &Path,
    cwd: &Path,
    tempdir: &Path,
    state_dir: &Path,
    argv: &[String],
) -> Vec<String> {
    let text = |path: &Path| path.to_string_lossy().into_owned();
    let mut wrapped = vec![
        text(bwrap),
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
    ];
    let mut bind = |dir: &Path| {
        wrapped.push("--bind".to_string());
        wrapped.push(text(dir));
        wrapped.push(text(dir));
    };
    bind(cwd);
    bind(tempdir);
    if !is_under(state_dir, cwd) {
        bind(state_dir);
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

fn is_under(path: &Path, root: &Path) -> bool {
    let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    real(path).starts_with(real(root))
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
    use std::ffi::OsString;

    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        list.iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
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
            Err(format!(
                "sandbox is not available: bwrap not found in PATH. {INSTALL_HINT}"
            ))
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
    fn pi_state_dir_follows_variable_then_home() {
        let cwd = Path::new("/work");
        let explicit = Session::assemble(
            Runtime::Pi,
            &pairs(&[("PI_CODING_AGENT_DIR", "state"), ("HOME", "/home/u")]),
            &[],
            &[],
            &[],
        );
        assert_eq!(
            pi_state_dir(&explicit, cwd),
            Ok(PathBuf::from("/work/state"))
        );
        let home = Session::assemble(
            Runtime::Pi,
            &pairs(&[("PI_CODING_AGENT_DIR", ""), ("HOME", "/home/u")]),
            &[],
            &[],
            &[],
        );
        assert_eq!(
            pi_state_dir(&home, cwd),
            Ok(PathBuf::from("/home/u/.pi/agent"))
        );
        let none = Session::assemble(Runtime::Pi, &[], &[], &[], &[]);
        assert_eq!(
            pi_state_dir(&none, cwd),
            Err("cannot create pi state directory $HOME/.pi/agent: HOME is not set".to_string())
        );
    }

    #[test]
    fn state_dir_is_created_privately_and_existing_kept() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a/b/agent");
        create_pi_state_dir(&dir).unwrap();
        for created in [root.path().join("a"), root.path().join("a/b"), dir.clone()] {
            let mode = std::fs::metadata(&created).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", created.display());
        }
        let open = root.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_pi_state_dir(&open).unwrap();
        let mode = std::fs::metadata(&open).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        let blocker = root.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        let blocked = blocker.join("agent");
        let detail = create_pi_state_dir(&blocked).unwrap_err();
        assert!(
            detail.starts_with(&format!(
                "cannot create pi state directory {}: ",
                blocked.display()
            )),
            "{detail}"
        );
    }

    #[test]
    fn wrapped_argv_binds_state_dir_outside_cwd() {
        let argv = strings(&["/usr/bin/pi", "-p", "hi"]);
        let outside = wrap_pi(
            Path::new("/usr/bin/bwrap"),
            Path::new("/nonexistent/work"),
            Path::new("/nonexistent/tmp/agentrun-x"),
            Path::new("/nonexistent/home/.pi/agent"),
            &argv,
        );
        assert_eq!(
            outside,
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
                "/nonexistent/home/.pi/agent",
                "/nonexistent/home/.pi/agent",
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
        let inside = wrap_pi(
            Path::new("/usr/bin/bwrap"),
            Path::new("/nonexistent/work"),
            Path::new("/nonexistent/tmp/agentrun-x"),
            Path::new("/nonexistent/work/.pi"),
            &argv,
        );
        assert_eq!(inside.iter().filter(|arg| *arg == "--bind").count(), 2);
        assert!(!inside.contains(&"/nonexistent/work/.pi".to_string()));
        let sibling = wrap_pi(
            Path::new("/usr/bin/bwrap"),
            Path::new("/a/b"),
            Path::new("/t"),
            Path::new("/a/bc"),
            &argv,
        );
        assert_eq!(sibling.iter().filter(|arg| *arg == "--bind").count(), 3);
    }

    #[test]
    fn wrapped_argv_compares_real_paths_but_writes_given_ones() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        std::fs::create_dir_all(work.join("state")).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&work, &link).unwrap();
        let linked_state = link.join("state");
        let argv = strings(&["pi"]);
        let wrapped = wrap_pi(
            Path::new("/usr/bin/bwrap"),
            &work,
            root.path(),
            &linked_state,
            &argv,
        );
        assert_eq!(wrapped.iter().filter(|arg| *arg == "--bind").count(), 2);
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let elsewhere_link = work.join("to-elsewhere");
        std::os::unix::fs::symlink(&elsewhere, &elsewhere_link).unwrap();
        let wrapped = wrap_pi(
            Path::new("/usr/bin/bwrap"),
            &work,
            root.path(),
            &elsewhere_link,
            &argv,
        );
        assert_eq!(wrapped.iter().filter(|arg| *arg == "--bind").count(), 3);
        assert!(wrapped.contains(&elsewhere_link.to_string_lossy().into_owned()));
        assert!(!wrapped.contains(&elsewhere.to_string_lossy().into_owned()));
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
