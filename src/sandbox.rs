use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::{Runtime, SandboxMode};
use crate::output::SandboxKind;
use crate::process_tree;
use crate::session::Session;
use crate::signal::Signals;

pub const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const CHECK_POLL: Duration = Duration::from_millis(10);
pub const BWRAP_PREFIX: &str = "bwrap: ";
pub const CANNOT_START_HINT: &str = "In a docker container use --sandbox off. On Ubuntu 23.10 or later, allow bwrap to create user namespaces with an AppArmor profile: https://yhnocoder.github.io/agentrun/pages/isolation.html#apparmor";
const SANDBOX_EXEC_PREFIX: &str = "sandbox-exec: ";
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
pub const UNAVAILABLE_PREFIX: &str = "sandbox is not available: ";
pub const SEATBELT_FILE: &str = "seatbelt.sb";
const FORWARD_SCRIPT: &str = r#""$0" "TCP-LISTEN:$1,bind=127.0.0.1,fork,reuseaddr" "UNIX-CONNECT:$2" 2>/dev/null & shift 2; exec "$@""#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sandbox {
    pub mode: SandboxMode,
    pub kind: SandboxKind,
    pub reason: String,
    pub bwrap: Option<PathBuf>,
    pub socat: Option<PathBuf>,
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
    socat: Option<PathBuf>,
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
            socat: None,
            description: "none (--sandbox off)".to_string(),
        });
    }
    match probe(runtime, session, cwd, signals) {
        Ok(available) => Ok(Sandbox {
            mode,
            kind: available.kind,
            reason: String::new(),
            bwrap: available.bwrap,
            socat: available.socat,
            description: available.description,
        }),
        Err(unavailable) if mode == SandboxMode::On => Err(format!(
            "{UNAVAILABLE_PREFIX}{}. {}",
            unavailable.reason, unavailable.hint
        )),
        Err(unavailable) => Ok(Sandbox {
            mode,
            kind: SandboxKind::None,
            description: format!("none (--sandbox relax: {})", unavailable.reason),
            reason: unavailable.reason,
            bwrap: None,
            socat: None,
        }),
    }
}

#[cfg(target_os = "linux")]
use bubblewrap::probe;

#[cfg(target_os = "macos")]
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
            socat: None,
            description: "codex".to_string(),
        });
    }
    let unavailable = |reason: String| Unavailable {
        reason,
        hint: "Use --sandbox relax or --sandbox off",
    };
    if !Path::new(SANDBOX_EXEC).exists() {
        return Err(unavailable(format!(
            "sandbox-exec not found at {SANDBOX_EXEC}"
        )));
    }
    let started = Instant::now();
    start_check(
        Path::new(SANDBOX_EXEC),
        &["-p", "(version 1)(allow default)", "/usr/bin/true"],
        SANDBOX_EXEC_PREFIX,
        session,
        cwd,
        signals,
    )
    .map_err(|reason| unavailable(format!("sandbox-exec cannot start: {reason}")))?;
    Ok(Available {
        kind: SandboxKind::Seatbelt,
        bwrap: None,
        socat: None,
        description: format!(
            "seatbelt ({SANDBOX_EXEC}, check {}ms)",
            started.elapsed().as_millis()
        ),
    })
}

#[cfg(target_os = "linux")]
mod bubblewrap {
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use super::{Available, BWRAP_PREFIX, CANNOT_START_HINT, Unavailable, start_check};
    use crate::cli::Runtime;
    use crate::output::SandboxKind;
    use crate::session::{Session, find_executable, is_executable};
    use crate::signal::Signals;

    const INSTALL_HINT: &str = "Install bubblewrap and socat (for example: apt-get install bubblewrap socat, or dnf install bubblewrap socat), or use --sandbox relax or --sandbox off";
    const CODEX_INSTALL_HINT: &str = "Install bubblewrap (for example: apt-get install bubblewrap, or dnf install bubblewrap), or use --sandbox relax or --sandbox off";

    pub(super) fn probe(
        runtime: Runtime,
        session: &Session,
        cwd: &Path,
        signals: &Signals,
    ) -> Result<Available, Unavailable> {
        let missing = |name: &str, hint: &'static str| Unavailable {
            reason: format!("{name} not found in PATH"),
            hint,
        };
        if runtime == Runtime::Codex {
            let bwrap = find_executable_outside("bwrap", &session.path, cwd)
                .ok_or_else(|| missing("bwrap", CODEX_INSTALL_HINT))?;
            let started = Instant::now();
            check_bwrap(&bwrap, session, cwd, signals)?;
            return Ok(Available {
                kind: SandboxKind::Codex,
                description: format!(
                    "codex (bubblewrap {}, check {}ms)",
                    bwrap.display(),
                    started.elapsed().as_millis()
                ),
                bwrap: Some(bwrap),
                socat: None,
            });
        }
        let bwrap = find_executable("bwrap", &session.path, cwd)
            .ok_or_else(|| missing("bwrap", INSTALL_HINT))?;
        let socat = find_executable("socat", &session.path, cwd)
            .ok_or_else(|| missing("socat", INSTALL_HINT))?;
        let started = Instant::now();
        check_bwrap(&bwrap, session, cwd, signals)?;
        Ok(Available {
            kind: SandboxKind::Bubblewrap,
            description: format!(
                "bubblewrap ({}, socat {}, check {}ms)",
                bwrap.display(),
                socat.display(),
                started.elapsed().as_millis()
            ),
            bwrap: Some(bwrap),
            socat: Some(socat),
        })
    }

    fn check_bwrap(
        bwrap: &Path,
        session: &Session,
        cwd: &Path,
        signals: &Signals,
    ) -> Result<(), Unavailable> {
        start_check(
            bwrap,
            &[
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--die-with-parent",
                "--unshare-net",
                "--",
                "/bin/true",
            ],
            BWRAP_PREFIX,
            session,
            cwd,
            signals,
        )
        .map_err(|reason| Unavailable {
            reason: format!("bwrap cannot start: {reason}"),
            hint: CANNOT_START_HINT,
        })
    }

    pub(super) fn find_executable_outside(name: &str, path: &OsStr, cwd: &Path) -> Option<PathBuf> {
        let cwd = std::path::absolute(cwd).ok()?;
        std::env::split_paths(path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .filter_map(|dir| std::path::absolute(cwd.join(dir)).ok())
            .filter(|dir| !dir.starts_with(&cwd))
            .map(|dir| dir.join(name))
            .find(|candidate| is_executable(candidate))
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
    while !process_tree::wait_for_exit(pid, false) {
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
    match exit_code {
        Some(code) => format!("exited with code {code}"),
        None => "terminated by a signal".to_string(),
    }
}

pub struct ProxyForward<'a> {
    pub socat: &'a Path,
    pub port: &'a str,
    pub socket: &'a Path,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiState {
    pub dir: PathBuf,
    pub writable: Vec<PathBuf>,
    pub login_target: Option<PathBuf>,
    pub readonly: Vec<PathBuf>,
}

pub fn wrap_pi(
    bwrap: &Path,
    cwd: &Path,
    tempdir: &Path,
    state: Option<&PiState>,
    forward: Option<&ProxyForward>,
    argv: &[String],
) -> Vec<String> {
    let text = |path: &Path| path.to_string_lossy().into_owned();
    let mut wrapped = vec![
        text(bwrap),
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
    ];
    let mut mount = |option: &str, path: &Path| {
        wrapped.push(option.to_string());
        wrapped.push(text(path));
        wrapped.push(text(path));
    };
    mount("--bind", cwd);
    mount("--bind", tempdir);
    if let Some(state) = state {
        mount("--bind", &state.dir);
        for entry in &state.readonly {
            mount("--ro-bind", entry);
        }
        if let Some(login_target) = &state.login_target {
            mount("--bind", login_target);
        }
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
    if let Some(forward) = forward {
        wrapped.extend([
            "/bin/sh".to_string(),
            "-c".to_string(),
            FORWARD_SCRIPT.to_string(),
            text(forward.socat),
            forward.port.to_string(),
            text(forward.socket),
        ]);
    }
    wrapped.extend(argv.iter().cloned());
    wrapped
}

pub fn seatbelt_profile(
    cwd: &Path,
    tempdir: &Path,
    state: Option<&PiState>,
    port: Option<&str>,
) -> String {
    let quoted = |path: &Path| {
        let text = path.to_string_lossy();
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    };
    let mut profile =
        String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*\n");
    profile.push_str(&format!("  (subpath {})\n", quoted(cwd)));
    profile.push_str(&format!("  (subpath {})\n", quoted(tempdir)));
    if let Some(state) = state {
        for path in &state.writable {
            profile.push_str(&format!("  (literal {})\n", quoted(path)));
        }
        if let Some(login_target) = &state.login_target {
            profile.push_str(&format!("  (literal {})\n", quoted(login_target)));
        }
    }
    profile.push_str(concat!(
        "  (literal \"/dev/null\")\n",
        "  (literal \"/dev/zero\")\n",
        "  (literal \"/dev/tty\")\n",
        "  (regex #\"^/dev/ttys[0-9]+$\")\n",
        "  (literal \"/dev/dtracehelper\"))\n",
        "(deny network*)\n",
    ));
    if let Some(port) = port {
        profile.push_str(&format!(
            "(allow network-outbound (remote tcp \"localhost:{port}\"))\n"
        ));
    }
    profile
}

pub fn write_seatbelt_profile(
    cwd: &Path,
    tempdir: &Path,
    state: Option<&PiState>,
    port: Option<&str>,
) -> std::io::Result<()> {
    let profile = seatbelt_profile(
        &std::fs::canonicalize(cwd)?,
        &std::fs::canonicalize(tempdir)?,
        state,
        port,
    );
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tempdir.join(SEATBELT_FILE))?
        .write_all(profile.as_bytes())
}

pub fn wrap_seatbelt(tempdir: &Path, argv: &[String]) -> Vec<String> {
    let mut wrapped = vec![
        SANDBOX_EXEC.to_string(),
        "-f".to_string(),
        tempdir.join(SEATBELT_FILE).to_string_lossy().into_owned(),
    ];
    wrapped.extend(argv.iter().cloned());
    wrapped
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

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_is_available_on_macos_and_codex_uses_its_own() {
        let signals = Signals::install();
        let session = Session::assemble(Runtime::Pi, &[], &[], &[], &[]);
        for runtime in [Runtime::Pi, Runtime::ClaudeCode] {
            let sandbox =
                check(SandboxMode::On, runtime, &session, Path::new("/"), &signals).unwrap();
            assert_eq!(sandbox.kind, SandboxKind::Seatbelt);
            assert_eq!(sandbox.bwrap, None);
            assert_eq!(sandbox.socat, None);
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
        assert_eq!(codex.kind, SandboxKind::Codex);
        assert_eq!(codex.description, "codex");
    }

    fn state(dir: &str, login_target: Option<&str>, readonly: &[&str]) -> PiState {
        PiState {
            dir: PathBuf::from(dir),
            writable: [
                "auth.json",
                "auth.json.lock",
                "models-store.json",
                "models-store.json.lock",
                "settings.json.lock",
            ]
            .iter()
            .map(|name| Path::new(dir).join(name))
            .collect(),
            login_target: login_target.map(PathBuf::from),
            readonly: readonly.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn seatbelt_profile_allows_writes_to_the_given_paths_and_the_proxy_port() {
        let state = state(
            "/Users/me/.pi/agent",
            Some("/Users/me/secrets/pi-auth.json"),
            &["/Users/me/.pi/agent/settings.json"],
        );
        let profile = seatbelt_profile(
            Path::new("/private/var/work"),
            Path::new("/private/tmp/agentrun-x"),
            Some(&state),
            Some("41234"),
        );
        assert_eq!(
            profile,
            concat!(
                "(version 1)\n",
                "(allow default)\n",
                "(deny file-write*)\n",
                "(allow file-write*\n",
                "  (subpath \"/private/var/work\")\n",
                "  (subpath \"/private/tmp/agentrun-x\")\n",
                "  (literal \"/Users/me/.pi/agent/auth.json\")\n",
                "  (literal \"/Users/me/.pi/agent/auth.json.lock\")\n",
                "  (literal \"/Users/me/.pi/agent/models-store.json\")\n",
                "  (literal \"/Users/me/.pi/agent/models-store.json.lock\")\n",
                "  (literal \"/Users/me/.pi/agent/settings.json.lock\")\n",
                "  (literal \"/Users/me/secrets/pi-auth.json\")\n",
                "  (literal \"/dev/null\")\n",
                "  (literal \"/dev/zero\")\n",
                "  (literal \"/dev/tty\")\n",
                "  (regex #\"^/dev/ttys[0-9]+$\")\n",
                "  (literal \"/dev/dtracehelper\"))\n",
                "(deny network*)\n",
                "(allow network-outbound (remote tcp \"localhost:41234\"))\n",
            )
        );
    }

    #[test]
    fn seatbelt_profile_without_state_or_port_and_with_quotes_in_paths() {
        let profile = seatbelt_profile(Path::new("/work/a\"b"), Path::new("/tmp/c\\d"), None, None);
        assert!(
            profile.contains("  (subpath \"/work/a\\\"b\")\n  (subpath \"/tmp/c\\\\d\")\n  (literal \"/dev/null\")\n"),
            "{profile}"
        );
        assert!(!profile.contains("auth.json"), "{profile}");
        assert!(profile.ends_with("(deny network*)\n"), "{profile}");
    }

    #[test]
    fn seatbelt_profile_with_a_plain_login_file_lists_only_the_five_entries() {
        let state = state("/Users/me/.pi/agent", None, &["/Users/me/.pi/agent/bin"]);
        let profile = seatbelt_profile(Path::new("/work"), Path::new("/tmp/s"), Some(&state), None);
        assert_eq!(
            profile.matches("(literal \"/Users/me/").count(),
            5,
            "{profile}"
        );
        assert!(!profile.contains("/bin"), "{profile}");
    }

    #[test]
    fn written_seatbelt_profile_uses_real_paths_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(root.path()).unwrap();
        let work = real.join("work");
        let session = real.join("session");
        std::fs::create_dir(&work).unwrap();
        std::fs::create_dir(&session).unwrap();
        std::os::unix::fs::symlink(&work, real.join("work-link")).unwrap();
        std::os::unix::fs::symlink(&session, real.join("session-link")).unwrap();
        write_seatbelt_profile(
            &real.join("work-link"),
            &real.join("session-link"),
            None,
            Some("7"),
        )
        .unwrap();
        let path = session.join("seatbelt.sb");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            seatbelt_profile(&work, &session, None, Some("7"))
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn seatbelt_wrapper_runs_the_command_after_the_profile() {
        assert_eq!(
            wrap_seatbelt(Path::new("<tempdir>"), &strings(&["/opt/bin/pi", "-p"])),
            strings(&[
                "/usr/bin/sandbox-exec",
                "-f",
                "<tempdir>/seatbelt.sb",
                "/opt/bin/pi",
                "-p"
            ])
        );
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
                socat: None,
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

    #[cfg(target_os = "linux")]
    #[test]
    fn codex_ignores_bwrap_inside_the_working_directory() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let inside = work.join("bin");
        let outside = root.path().join("bin");
        for dir in [&inside, &outside] {
            std::fs::create_dir_all(dir).unwrap();
            let file = dir.join("bwrap");
            std::fs::write(&file, "#!/bin/sh\nexit 1\n").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path =
            std::env::join_paths(["bin", inside.to_str().unwrap(), outside.to_str().unwrap()])
                .unwrap();
        assert_eq!(
            bubblewrap::find_executable_outside("bwrap", &path, &work),
            Some(outside.join("bwrap"))
        );
        let only_inside = std::env::join_paths(["bin"]).unwrap();
        assert_eq!(
            bubblewrap::find_executable_outside("bwrap", &only_inside, &work),
            None
        );
    }

    #[test]
    fn wrapped_argv_binds_cwd_tempdir_and_the_state_dir_in_order() {
        let argv = strings(&["/usr/bin/pi", "-p", "hi"]);
        let state = state(
            "/nonexistent/home/.pi/agent",
            Some("/nonexistent/secrets/pi-auth.json"),
            &[
                "/nonexistent/home/.pi/agent/bin",
                "/nonexistent/home/.pi/agent/settings.json",
            ],
        );
        let with_state = wrap_pi(
            Path::new("/usr/bin/bwrap"),
            Path::new("/nonexistent/work"),
            Path::new("/nonexistent/tmp/agentrun-x"),
            Some(&state),
            None,
            &argv,
        );
        assert_eq!(
            with_state,
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
                "--ro-bind",
                "/nonexistent/home/.pi/agent/bin",
                "/nonexistent/home/.pi/agent/bin",
                "--ro-bind",
                "/nonexistent/home/.pi/agent/settings.json",
                "/nonexistent/home/.pi/agent/settings.json",
                "--bind",
                "/nonexistent/secrets/pi-auth.json",
                "/nonexistent/secrets/pi-auth.json",
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
        let without_state = wrap_pi(
            Path::new("/usr/bin/bwrap"),
            Path::new("/nonexistent/work"),
            Path::new("/nonexistent/tmp/agentrun-x"),
            None,
            None,
            &argv,
        );
        assert_eq!(
            without_state.iter().filter(|arg| *arg == "--bind").count(),
            2
        );
        assert_eq!(without_state.len(), with_state.len() - 12);
    }

    #[test]
    fn wrapped_argv_with_forward_starts_socat_before_pi() {
        let argv = strings(&["/usr/bin/pi", "-p", "hi"]);
        let forward = ProxyForward {
            socat: Path::new("/usr/bin/socat"),
            port: "41234",
            socket: Path::new("/nonexistent/tmp/agentrun-x/proxy.sock"),
        };
        let wrapped = wrap_pi(
            Path::new("/usr/bin/bwrap"),
            Path::new("/nonexistent/work"),
            Path::new("/nonexistent/tmp/agentrun-x"),
            None,
            Some(&forward),
            &argv,
        );
        let separator = wrapped.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(
            wrapped[separator + 1..],
            strings(&[
                "/bin/sh",
                "-c",
                r#""$0" "TCP-LISTEN:$1,bind=127.0.0.1,fork,reuseaddr" "UNIX-CONNECT:$2" 2>/dev/null & shift 2; exec "$@""#,
                "/usr/bin/socat",
                "41234",
                "/nonexistent/tmp/agentrun-x/proxy.sock",
                "/usr/bin/pi",
                "-p",
                "hi",
            ])
        );
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
