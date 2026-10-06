#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::time::Instant;

use super::PiState;
#[cfg(target_os = "linux")]
use super::{Available, Unavailable, Wrapper, start_check};
#[cfg(target_os = "linux")]
use crate::cli::Runtime;
#[cfg(target_os = "linux")]
use crate::run::Signals;
#[cfg(target_os = "linux")]
use crate::session::{Session, find_executable, is_executable};

pub const BWRAP_PREFIX: &str = "bwrap: ";
pub const CANNOT_START_HINT: &str = "In a docker container use --sandbox off. On Ubuntu 23.10 or later, allow bwrap to create user namespaces with an AppArmor profile: https://yhnocoder.github.io/agentrun/pages/isolation.html#apparmor";
const FORWARD_SCRIPT: &str = r#""$0" "TCP-LISTEN:$1,bind=127.0.0.1,fork,reuseaddr" "UNIX-CONNECT:$2" 2>/dev/null & shift 2; exec "$@""#;
#[cfg(target_os = "linux")]
const INSTALL_HINT: &str = "Install bubblewrap and socat (for example: apt-get install bubblewrap socat, or dnf install bubblewrap socat), or use --sandbox relax or --sandbox off";
#[cfg(target_os = "linux")]
const CODEX_INSTALL_HINT: &str = "Install bubblewrap (for example: apt-get install bubblewrap, or dnf install bubblewrap), or use --sandbox relax or --sandbox off";

pub struct ProxyForward<'a> {
    pub socat: &'a Path,
    pub port: &'a str,
    pub socket: &'a Path,
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

#[cfg(target_os = "linux")]
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
            wrapper: Wrapper::Codex,
            description: format!(
                "codex (bubblewrap {}, check {}ms)",
                bwrap.display(),
                started.elapsed().as_millis()
            ),
        });
    }
    let bwrap = find_executable("bwrap", &session.path, cwd)
        .ok_or_else(|| missing("bwrap", INSTALL_HINT))?;
    let socat = find_executable("socat", &session.path, cwd)
        .ok_or_else(|| missing("socat", INSTALL_HINT))?;
    let started = Instant::now();
    check_bwrap(&bwrap, session, cwd, signals)?;
    Ok(Available {
        description: format!(
            "bubblewrap ({}, socat {}, check {}ms)",
            bwrap.display(),
            socat.display(),
            started.elapsed().as_millis()
        ),
        wrapper: Wrapper::Bubblewrap { bwrap, socat },
    })
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn find_executable_outside(name: &str, path: &OsStr, cwd: &Path) -> Option<PathBuf> {
    let cwd = std::path::absolute(cwd).ok()?;
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .filter_map(|dir| std::path::absolute(cwd.join(dir)).ok())
        .filter(|dir| !dir.starts_with(&cwd))
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| item.to_string()).collect()
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
            find_executable_outside("bwrap", &path, &work),
            Some(outside.join("bwrap"))
        );
        let only_inside = std::env::join_paths(["bin"]).unwrap();
        assert_eq!(find_executable_outside("bwrap", &only_inside, &work), None);
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
}
