use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::cli::Runtime;
use crate::runtime::{codex, pi};
use crate::session::{CODEX_AUTH_VARIABLE, PI_AUTH_VARIABLE, Session, home_placeholder};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialStatus {
    Written,
    Unchanged,
}

impl CredentialStatus {
    pub fn name(self) -> &'static str {
        match self {
            CredentialStatus::Written => "written",
            CredentialStatus::Unchanged => "unchanged",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialReport {
    pub variable: &'static str,
    pub path: PathBuf,
    pub status: CredentialStatus,
}

pub fn write_session_credential(
    runtime: Runtime,
    session: &Session,
    cwd: &Path,
) -> Result<Option<CredentialReport>, String> {
    let (variable, value, dir, home_subdir, login_file) = match runtime {
        Runtime::Codex => (
            CODEX_AUTH_VARIABLE,
            &session.codex_auth,
            codex::user_home(session, cwd),
            codex::HOME_SUBDIR,
            codex::LOGIN_FILE,
        ),
        Runtime::Pi => (
            PI_AUTH_VARIABLE,
            &session.pi_auth,
            pi::user_state_dir(session, cwd),
            pi::STATE_HOME_SUBDIR,
            pi::LOGIN_FILE,
        ),
        Runtime::ClaudeCode => return Ok(None),
    };
    let Some(value) = value.as_ref().filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let Some(dir) = dir else {
        return Err(format!(
            "cannot write {variable} to {}: HOME is not set",
            home_placeholder(home_subdir).join(login_file).display()
        ));
    };
    let path = dir.join(login_file);
    let status = write_credential(&path, value.as_bytes())
        .map_err(|error| format!("cannot write {variable} to {}: {error}", path.display()))?;
    Ok(Some(CredentialReport {
        variable,
        path,
        status,
    }))
}

fn write_credential(path: &Path, value: &[u8]) -> std::io::Result<CredentialStatus> {
    let digest: String = Sha256::digest(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .chain(std::iter::once("\n".to_string()))
        .collect();
    let linked = path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
        .then(|| std::fs::canonicalize(path).ok())
        .flatten();
    let target = match linked {
        Some(target) => target,
        None => {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path.parent().unwrap_or(Path::new(".")))?;
            path.to_path_buf()
        }
    };
    let _lock = lock_exclusively(target.parent().unwrap_or(Path::new(".")))?;
    let sidecar = sidecar_of(&target);
    if target.exists() && std::fs::read_to_string(&sidecar).is_ok_and(|recorded| recorded == digest)
    {
        return Ok(CredentialStatus::Unchanged);
    }
    write_atomically(&target, value)?;
    write_atomically(&sidecar, digest.as_bytes())?;
    Ok(CredentialStatus::Written)
}

fn sidecar_of(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".agentrun-sha256");
    target.with_file_name(name)
}

fn lock_exclusively(dir: &Path) -> std::io::Result<File> {
    let handle = File::open(dir)?;
    loop {
        if unsafe { libc::flock(handle.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(handle);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn write_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (temp, mut file) = loop {
        let temp = dir.join(format!(".{name}.agentrun-{}", temp_suffix()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
        {
            Ok(file) => break (temp, file),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let written = file
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .and_then(|()| file.write_all(contents))
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&temp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

fn temp_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let count = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos:x}-{count}", std::process::id())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::Arc;

    use super::*;

    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n";
    const LOGIN_FILE: &str = "auth.json";
    const SIDECAR_FILE: &str = "auth.json.agentrun-sha256";

    fn session(runtime: Runtime, env: &[(&str, &str)]) -> Session {
        let env: Vec<(OsString, OsString)> = env
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect();
        Session::assemble(runtime, &env, &[], &[], &[])
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn sidecar(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(SIDECAR_FILE)).unwrap()
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn first_write_creates_login_file_sidecar_and_private_directories() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a/b/agent");
        let login = dir.join(LOGIN_FILE);
        assert_eq!(
            write_credential(&login, b"abc").unwrap(),
            CredentialStatus::Written
        );
        assert_eq!(std::fs::read(&login).unwrap(), b"abc");
        assert_eq!(sidecar(&dir), ABC_SHA256);
        assert_eq!(mode(&login), 0o600);
        assert_eq!(mode(&dir.join(SIDECAR_FILE)), 0o600);
        for created in [root.path().join("a"), root.path().join("a/b"), dir.clone()] {
            assert_eq!(mode(&created), 0o700, "{}", created.display());
        }
        assert_eq!(entries(&dir), [LOGIN_FILE, SIDECAR_FILE]);
    }

    #[test]
    fn existing_directory_keeps_its_permissions() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("existing");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_credential(&dir.join(LOGIN_FILE), b"abc").unwrap();
        assert_eq!(mode(&dir), 0o755);
    }

    #[test]
    fn same_value_leaves_refreshed_login_file_alone() {
        let root = tempfile::tempdir().unwrap();
        let login = root.path().join(LOGIN_FILE);
        write_credential(&login, b"abc").unwrap();
        std::fs::write(&login, "refreshed").unwrap();
        assert_eq!(
            write_credential(&login, b"abc").unwrap(),
            CredentialStatus::Unchanged
        );
        assert_eq!(std::fs::read_to_string(&login).unwrap(), "refreshed");
    }

    #[test]
    fn different_value_overwrites_with_private_mode() {
        let root = tempfile::tempdir().unwrap();
        let login = root.path().join(LOGIN_FILE);
        write_credential(&login, b"old").unwrap();
        std::fs::set_permissions(&login, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            write_credential(&login, b"abc").unwrap(),
            CredentialStatus::Written
        );
        assert_eq!(std::fs::read(&login).unwrap(), b"abc");
        assert_eq!(sidecar(root.path()), ABC_SHA256);
        assert_eq!(mode(&login), 0o600);
    }

    #[test]
    fn missing_sidecar_overwrites_existing_login_file() {
        let root = tempfile::tempdir().unwrap();
        let login = root.path().join(LOGIN_FILE);
        std::fs::write(&login, "user login").unwrap();
        assert_eq!(
            write_credential(&login, b"abc").unwrap(),
            CredentialStatus::Written
        );
        assert_eq!(std::fs::read(&login).unwrap(), b"abc");
        assert_eq!(sidecar(root.path()), ABC_SHA256);
    }

    #[test]
    fn missing_login_file_is_written_even_with_matching_sidecar() {
        let root = tempfile::tempdir().unwrap();
        let login = root.path().join(LOGIN_FILE);
        std::fs::write(root.path().join(SIDECAR_FILE), ABC_SHA256).unwrap();
        assert_eq!(
            write_credential(&login, b"abc").unwrap(),
            CredentialStatus::Written
        );
        assert_eq!(std::fs::read(&login).unwrap(), b"abc");
    }

    #[test]
    fn linked_login_file_is_written_through_the_link() {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared.json");
        std::fs::write(&shared, "old").unwrap();
        let state = root.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let login = state.join(LOGIN_FILE);
        std::os::unix::fs::symlink(&shared, &login).unwrap();
        assert_eq!(
            write_credential(&login, b"abc").unwrap(),
            CredentialStatus::Written
        );
        assert!(login.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&shared).unwrap(), b"abc");
        assert_eq!(mode(&shared), 0o600);
        assert_eq!(
            std::fs::read_to_string(root.path().join("shared.json.agentrun-sha256")).unwrap(),
            ABC_SHA256
        );
        assert_eq!(entries(&state), [LOGIN_FILE]);
    }

    #[test]
    fn dangling_link_is_replaced_by_the_login_file() {
        let root = tempfile::tempdir().unwrap();
        let login = root.path().join(LOGIN_FILE);
        std::os::unix::fs::symlink(root.path().join("missing.json"), &login).unwrap();
        assert_eq!(
            write_credential(&login, b"abc").unwrap(),
            CredentialStatus::Written
        );
        assert!(login.symlink_metadata().unwrap().file_type().is_file());
        assert_eq!(std::fs::read(&login).unwrap(), b"abc");
        assert_eq!(sidecar(root.path()), ABC_SHA256);
        assert_eq!(entries(root.path()), [LOGIN_FILE, SIDECAR_FILE]);
    }

    #[test]
    fn two_state_dirs_linked_to_one_login_file_share_one_sidecar() {
        let root = tempfile::tempdir().unwrap();
        let shared_dir = root.path().join("shared");
        std::fs::create_dir(&shared_dir).unwrap();
        let shared = shared_dir.join(LOGIN_FILE);
        std::fs::write(&shared, "old").unwrap();
        let logins: Vec<PathBuf> = ["first", "second"]
            .iter()
            .map(|name| {
                let state = root.path().join(name);
                std::fs::create_dir(&state).unwrap();
                let login = state.join(LOGIN_FILE);
                std::os::unix::fs::symlink(&shared, &login).unwrap();
                login
            })
            .collect();
        assert_eq!(
            write_credential(&logins[0], b"abc").unwrap(),
            CredentialStatus::Written
        );
        assert_eq!(
            write_credential(&logins[1], b"abc").unwrap(),
            CredentialStatus::Unchanged
        );
        assert_eq!(entries(&shared_dir), [LOGIN_FILE, SIDECAR_FILE]);
        assert_eq!(sidecar(&shared_dir), ABC_SHA256);
        for login in &logins {
            assert_eq!(entries(login.parent().unwrap()), [LOGIN_FILE]);
        }
    }

    #[test]
    fn concurrent_writes_leave_one_complete_value_and_no_temp_files() {
        let root = tempfile::tempdir().unwrap();
        let login = Arc::new(root.path().join("shared").join(LOGIN_FILE));
        let values: Vec<Vec<u8>> = (0..16u8)
            .map(|index| vec![b'a' + index; 64 * 1024])
            .collect();
        let handles: Vec<_> = values
            .iter()
            .cloned()
            .map(|value| {
                let login = Arc::clone(&login);
                std::thread::spawn(move || write_credential(&login, &value).unwrap())
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let content = std::fs::read(login.as_ref()).unwrap();
        assert!(values.contains(&content));
        assert_eq!(entries(login.parent().unwrap()), [LOGIN_FILE, SIDECAR_FILE]);
    }

    #[test]
    fn pi_credential_goes_to_agent_dir_resolved_from_cwd() {
        let root = tempfile::tempdir().unwrap();
        let session = session(
            Runtime::Pi,
            &[
                ("AGENTRUN_PI_AUTH", "abc"),
                ("PI_CODING_AGENT_DIR", "rel/agent"),
                ("HOME", "/nonexistent-home"),
            ],
        );
        let report = write_session_credential(Runtime::Pi, &session, root.path())
            .unwrap()
            .unwrap();
        let expected = root.path().join("rel/agent").join(LOGIN_FILE);
        assert_eq!(
            report,
            CredentialReport {
                variable: "AGENTRUN_PI_AUTH",
                path: expected.clone(),
                status: CredentialStatus::Written,
            }
        );
        assert_eq!(std::fs::read(&expected).unwrap(), b"abc");
        let again = write_session_credential(Runtime::Pi, &session, root.path())
            .unwrap()
            .unwrap();
        assert_eq!(again.status, CredentialStatus::Unchanged);
    }

    #[test]
    fn codex_credential_goes_to_codex_home_resolved_from_cwd() {
        let root = tempfile::tempdir().unwrap();
        let session = session(
            Runtime::Codex,
            &[("AGENTRUN_CODEX_AUTH", "abc"), ("CODEX_HOME", "codex")],
        );
        let report = write_session_credential(Runtime::Codex, &session, root.path())
            .unwrap()
            .unwrap();
        assert_eq!(report.variable, "AGENTRUN_CODEX_AUTH");
        assert_eq!(report.path, root.path().join("codex").join(LOGIN_FILE));
        assert_eq!(std::fs::read(&report.path).unwrap(), b"abc");
    }

    #[test]
    fn empty_directory_variables_fall_back_to_home() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let home_str = home.to_str().unwrap();
        let codex = session(
            Runtime::Codex,
            &[
                ("AGENTRUN_CODEX_AUTH", "abc"),
                ("CODEX_HOME", ""),
                ("HOME", home_str),
            ],
        );
        let report = write_session_credential(Runtime::Codex, &codex, root.path())
            .unwrap()
            .unwrap();
        assert_eq!(report.path, home.join(".codex").join(LOGIN_FILE));
        let pi = session(
            Runtime::Pi,
            &[("AGENTRUN_PI_AUTH", "abc"), ("HOME", home_str)],
        );
        let report = write_session_credential(Runtime::Pi, &pi, root.path())
            .unwrap()
            .unwrap();
        assert_eq!(report.path, home.join(".pi/agent").join(LOGIN_FILE));
        assert_eq!(mode(&home.join(".pi")), 0o700);
    }

    #[test]
    fn unset_home_is_a_write_failure_without_the_value() {
        let root = tempfile::tempdir().unwrap();
        for env in [
            vec![("AGENTRUN_PI_AUTH", "secret-value")],
            vec![("AGENTRUN_PI_AUTH", "secret-value"), ("HOME", "")],
        ] {
            let session = session(Runtime::Pi, &env);
            let detail = write_session_credential(Runtime::Pi, &session, root.path()).unwrap_err();
            assert_eq!(
                detail,
                "cannot write AGENTRUN_PI_AUTH to $HOME/.pi/agent/auth.json: HOME is not set"
            );
        }
        assert!(entries(root.path()).is_empty());
    }

    #[test]
    fn write_failure_names_variable_and_path_only() {
        let root = tempfile::tempdir().unwrap();
        let blocker = root.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        let session = session(
            Runtime::Codex,
            &[
                ("AGENTRUN_CODEX_AUTH", "secret-value"),
                ("CODEX_HOME", blocker.join("codex").to_str().unwrap()),
            ],
        );
        let detail = write_session_credential(Runtime::Codex, &session, root.path()).unwrap_err();
        let prefix = format!(
            "cannot write AGENTRUN_CODEX_AUTH to {}: ",
            blocker.join("codex").join(LOGIN_FILE).display()
        );
        assert!(detail.starts_with(&prefix), "{detail}");
        assert!(!detail.contains("secret-value"));
    }

    #[test]
    fn only_the_current_runtime_variable_is_written() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().to_str().unwrap();
        let env = [
            ("AGENTRUN_CODEX_AUTH", "codex"),
            ("AGENTRUN_PI_AUTH", ""),
            ("HOME", home),
        ];
        for runtime in [Runtime::ClaudeCode, Runtime::Pi] {
            let session = session(runtime, &env);
            assert_eq!(
                write_session_credential(runtime, &session, root.path()),
                Ok(None)
            );
        }
        assert!(entries(root.path()).is_empty());
    }
}
