use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::time::Instant;

use super::PiState;
#[cfg(target_os = "macos")]
use super::{Available, Unavailable, Wrapper, start_check};
#[cfg(target_os = "macos")]
use crate::cli::Runtime;
#[cfg(target_os = "macos")]
use crate::session::Session;

pub(super) const SANDBOX_EXEC_PREFIX: &str = "sandbox-exec: ";
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
pub const SEATBELT_FILE: &str = "seatbelt.sb";

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

#[cfg(target_os = "macos")]
pub(super) fn probe(
    runtime: Runtime,
    session: &Session,
    cwd: &Path,
    checking: &dyn Fn(Option<i32>),
) -> Result<Available, Unavailable> {
    if runtime == Runtime::Codex {
        return Ok(Available {
            wrapper: Wrapper::Codex,
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
        checking,
    )
    .map_err(|reason| unavailable(format!("sandbox-exec cannot start: {reason}")))?;
    Ok(Available {
        wrapper: Wrapper::Seatbelt,
        description: format!(
            "seatbelt ({SANDBOX_EXEC}, check {}ms)",
            started.elapsed().as_millis()
        ),
    })
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
}
