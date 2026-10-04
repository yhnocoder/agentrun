use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::cli::Runtime;

const AGENTRUN_PREFIX: &[u8] = b"AGENTRUN_";
const CODEX_REMOVED: [&str; 3] = ["CODEX_API_KEY", "OPENAI_API_KEY", "OPENAI_BASE_URL"];
const CLAUDE_CODE_REMOVED_PREFIXES: [&[u8]; 2] = [b"ANTHROPIC_", b"CLAUDE_CODE_USE_"];

pub struct Session {
    pub env: BTreeMap<OsString, OsString>,
    pub path: OsString,
    pub set: Vec<String>,
    pub removed: Vec<String>,
    pub agentrun_variables: Vec<String>,
    pub sandbox: Option<OsString>,
    pub codex_auth: Option<OsString>,
    pub pi_auth: Option<OsString>,
}

impl Session {
    pub fn assemble(
        runtime: Runtime,
        caller_env: &[(OsString, OsString)],
        path_dirs: &[PathBuf],
        env_files: &[Vec<(String, String)>],
        env_args: &[(String, OsString)],
    ) -> Session {
        let mut env: BTreeMap<OsString, OsString> = caller_env.iter().cloned().collect();

        let removed_names: Vec<OsString> = env
            .keys()
            .filter(|name| removed_by(runtime, name))
            .cloned()
            .collect();
        for name in &removed_names {
            env.remove(name);
        }

        let mut set: Vec<String> = Vec::new();
        let explicit = env_files
            .iter()
            .flatten()
            .map(|(key, value)| (key, OsString::from(value)))
            .chain(env_args.iter().map(|(key, value)| (key, value.clone())));
        for (key, value) in explicit {
            if !key.as_bytes().starts_with(AGENTRUN_PREFIX) && !set.contains(key) {
                set.push(key.clone());
            }
            env.insert(OsString::from(key), value);
        }

        let sandbox = env.get(OsStr::new("AGENTRUN_SANDBOX")).cloned();
        let codex_auth = env.get(OsStr::new("AGENTRUN_CODEX_AUTH")).cloned();
        let pi_auth = env.get(OsStr::new("AGENTRUN_PI_AUTH")).cloned();
        let agentrun_names: Vec<OsString> = env
            .keys()
            .filter(|name| name.as_bytes().starts_with(AGENTRUN_PREFIX))
            .cloned()
            .collect();
        for name in &agentrun_names {
            env.remove(name);
        }

        let path = session_path(path_dirs, env.get(OsStr::new("PATH")));
        env.insert(OsString::from("PATH"), path.clone());

        Session {
            env,
            path,
            set,
            removed: lossy_names(&removed_names),
            agentrun_variables: lossy_names(&agentrun_names),
            sandbox,
            codex_auth,
            pi_auth,
        }
    }
}

fn removed_by(runtime: Runtime, name: &OsStr) -> bool {
    match runtime {
        Runtime::ClaudeCode => CLAUDE_CODE_REMOVED_PREFIXES
            .iter()
            .any(|prefix| name.as_bytes().starts_with(prefix)),
        Runtime::Codex => CODEX_REMOVED.iter().any(|removed| name == *removed),
        Runtime::Pi => false,
    }
}

fn lossy_names(names: &[OsString]) -> Vec<String> {
    names
        .iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

fn session_path(path_dirs: &[PathBuf], base: Option<&OsString>) -> OsString {
    let mut parts: Vec<&OsStr> = path_dirs.iter().map(|dir| dir.as_os_str()).collect();
    if let Some(base) = base.filter(|base| !base.is_empty()) {
        parts.push(base);
    }
    let mut path = OsString::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            path.push(":");
        }
        path.push(part);
    }
    path
}

pub fn is_variable_name(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn parse_env_args(
    args: &[String],
    caller_env: &[(OsString, OsString)],
) -> Result<Vec<(String, OsString)>, String> {
    args.iter()
        .map(|arg| {
            let (key, value) = match arg.split_once('=') {
                Some((key, value)) => (key, Some(OsString::from(value))),
                None => (arg.as_str(), None),
            };
            if !is_variable_name(key) {
                return Err(format!("--env: invalid variable name '{key}'"));
            }
            let value = match value {
                Some(value) => value,
                None => caller_env
                    .iter()
                    .rev()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
                    .ok_or_else(|| format!("--env {key}: not set in the caller's environment"))?,
            };
            Ok((key.to_string(), value))
        })
        .collect()
}

pub fn resolve_path_dirs(dirs: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    dirs.iter()
        .map(|dir| {
            std::path::absolute(dir)
                .ok()
                .filter(|absolute| absolute.is_dir())
                .ok_or_else(|| format!("--path {}: not a directory", dir.display()))
        })
        .collect()
}

pub fn read_env_file(path: &Path) -> Result<Vec<(String, String)>, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("--env-file {}: {error}", path.display()))?;
    let content = String::from_utf8(bytes)
        .map_err(|_| format!("--env-file {}: not valid UTF-8", path.display()))?;
    parse_env_file(&content).map_err(|(number, problem)| {
        format!("--env-file {}: line {number}: {problem}", path.display())
    })
}

fn parse_env_file(content: &str) -> Result<Vec<(String, String)>, (usize, &'static str)> {
    let mut entries = Vec::new();
    for (index, line) in content.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let malformed = (index + 1, "expected KEY=VALUE");
        let body = line.strip_prefix("export ").unwrap_or(line);
        if body.starts_with(char::is_whitespace) {
            return Err(malformed);
        }
        let Some((key, value)) = body.split_once('=') else {
            return Err(malformed);
        };
        if key.ends_with(char::is_whitespace) || value.starts_with(char::is_whitespace) {
            return Err(malformed);
        }
        if !is_variable_name(key) {
            return Err((index + 1, "invalid variable name"));
        }
        entries.push((key.to_string(), unquote(value).to_string()));
    }
    Ok(entries)
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    let paired = bytes.len() >= 2
        && bytes[0] == bytes[bytes.len() - 1]
        && (bytes[0] == b'\'' || bytes[0] == b'"');
    if paired {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

pub fn find_executable(name: &str, path: &OsStr, cwd: &Path) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| cwd.join(dir).join(name))
        .find(|candidate| is_executable(candidate))
        .and_then(|candidate| std::path::absolute(candidate).ok())
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        list.iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect()
    }

    fn file_entries(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn arg_entries(list: &[(&str, &str)]) -> Vec<(String, OsString)> {
        list.iter()
            .map(|(key, value)| (key.to_string(), OsString::from(value)))
            .collect()
    }

    fn var<'a>(session: &'a Session, name: &str) -> Option<&'a str> {
        session
            .env
            .get(OsStr::new(name))
            .and_then(|value| value.to_str())
    }

    fn executable(path: &Path) {
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn env_file_accepts_comments_quotes_and_export() {
        let content = concat!(
            "# comment\n",
            "\n",
            "   \n",
            "   # indented comment\n",
            "PLAIN=value\r\n",
            "export EXPORTED=yes\n",
            "SINGLE='hello world'\n",
            "DOUBLE=\"a \\n b\"\n",
            "LEFT='open\n",
            "RIGHT=close\"\n",
            "ONE='\n",
            "HASH=a#b # not a comment\n",
            "EQUALS=a=b=c\n",
            "EMPTY=\n",
            "EMPTY_QUOTES=''\n",
            "MIXED='x\"\n",
            "_under_1=x",
        );
        assert_eq!(
            parse_env_file(content).unwrap(),
            file_entries(&[
                ("PLAIN", "value"),
                ("EXPORTED", "yes"),
                ("SINGLE", "hello world"),
                ("DOUBLE", "a \\n b"),
                ("LEFT", "'open"),
                ("RIGHT", "close\""),
                ("ONE", "'"),
                ("HASH", "a#b # not a comment"),
                ("EQUALS", "a=b=c"),
                ("EMPTY", ""),
                ("EMPTY_QUOTES", ""),
                ("MIXED", "'x\""),
                ("_under_1", "x"),
            ])
        );
    }

    #[test]
    fn env_file_format_errors_report_line_number() {
        let malformed = [
            "A=1\nB = 2\n",
            "A=1\nB =2\n",
            "A=1\nB= 2\n",
            "A=1\n  B=2\n",
            "A=1\nNOVALUE\n",
            "A=1\nexport  B=2\n",
            "A=1\n\texport B=2\n",
        ];
        for content in malformed {
            assert_eq!(
                parse_env_file(content),
                Err((2, "expected KEY=VALUE")),
                "{content:?}"
            );
        }
        for content in ["# c\nA=1\n1BAD=x\n", "\n\nA=1\nB-C=x\n", "A=1\n\n=x\n"] {
            let line = content
                .split('\n')
                .position(|line| line.contains('x'))
                .unwrap()
                + 1;
            assert_eq!(
                parse_env_file(content),
                Err((line, "invalid variable name")),
                "{content:?}"
            );
        }
    }

    #[test]
    fn env_file_detail_has_path_and_line_without_content() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bad.env");
        std::fs::write(&file, "A=1\n\nSECRET_VALUE = hunter2\n").unwrap();
        let detail = read_env_file(&file).unwrap_err();
        assert_eq!(
            detail,
            format!("--env-file {}: line 3: expected KEY=VALUE", file.display())
        );
        std::fs::write(&file, "A=1\n9SECRET=hunter2\n").unwrap();
        let detail = read_env_file(&file).unwrap_err();
        assert_eq!(
            detail,
            format!(
                "--env-file {}: line 2: invalid variable name",
                file.display()
            )
        );
        assert!(!detail.contains("hunter2") && !detail.contains("9SECRET"));
    }

    #[test]
    fn env_file_must_be_readable_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("binary.env");
        std::fs::write(&file, [b'A', b'=', 0xff, b'\n']).unwrap();
        assert_eq!(
            read_env_file(&file).unwrap_err(),
            format!("--env-file {}: not valid UTF-8", file.display())
        );
        let missing = dir.path().join("missing.env");
        let detail = read_env_file(&missing).unwrap_err();
        assert!(
            detail.starts_with(&format!("--env-file {}: ", missing.display())),
            "{detail}"
        );
    }

    #[test]
    fn env_args_take_value_or_caller_value() {
        let caller = pairs(&[
            ("ANTHROPIC_API_KEY", "old"),
            ("ANTHROPIC_API_KEY", "caller"),
            ("EMPTY", ""),
        ]);
        let parsed = parse_env_args(
            &[
                "A=1".to_string(),
                "B=".to_string(),
                "C=x=y".to_string(),
                "ANTHROPIC_API_KEY".to_string(),
                "EMPTY".to_string(),
            ],
            &caller,
        )
        .unwrap();
        assert_eq!(
            parsed,
            arg_entries(&[
                ("A", "1"),
                ("B", ""),
                ("C", "x=y"),
                ("ANTHROPIC_API_KEY", "caller"),
                ("EMPTY", ""),
            ])
        );
    }

    #[test]
    fn env_args_reject_missing_and_invalid_names() {
        assert_eq!(
            parse_env_args(&["NO_SUCH_VAR".to_string()], &[]),
            Err("--env NO_SUCH_VAR: not set in the caller's environment".to_string())
        );
        for (arg, key) in [("1BAD=x", "1BAD"), ("A-B=x", "A-B"), ("=x", ""), ("", "")] {
            assert_eq!(
                parse_env_args(&[arg.to_string()], &[]),
                Err(format!("--env: invalid variable name '{key}'"))
            );
        }
    }

    #[test]
    fn claude_code_removes_api_variables_from_caller_only() {
        let caller = pairs(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("ANTHROPIC_X", "x"),
            ("CLAUDE_CODE_USE_X", "1"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "token"),
            ("OPENAI_API_KEY", "o"),
            ("HOME", "/home/u"),
        ]);
        let session = Session::assemble(
            Runtime::ClaudeCode,
            &caller,
            &[],
            &[],
            &arg_entries(&[("ANTHROPIC_BASE_URL", "https://example.test")]),
        );
        assert_eq!(
            session.removed,
            ["ANTHROPIC_API_KEY", "ANTHROPIC_X", "CLAUDE_CODE_USE_X"]
        );
        assert_eq!(var(&session, "ANTHROPIC_X"), None);
        assert_eq!(var(&session, "CLAUDE_CODE_USE_X"), None);
        assert_eq!(var(&session, "CLAUDE_CODE_OAUTH_TOKEN"), Some("token"));
        assert_eq!(var(&session, "OPENAI_API_KEY"), Some("o"));
        assert_eq!(
            var(&session, "ANTHROPIC_BASE_URL"),
            Some("https://example.test")
        );
        assert_eq!(session.set, ["ANTHROPIC_BASE_URL"]);
    }

    #[test]
    fn codex_removes_three_variables_and_pi_removes_none() {
        let caller = pairs(&[
            ("CODEX_API_KEY", "a"),
            ("OPENAI_API_KEY", "b"),
            ("OPENAI_BASE_URL", "c"),
            ("OPENAI_ORG", "d"),
            ("ANTHROPIC_API_KEY", "e"),
        ]);
        let codex = Session::assemble(Runtime::Codex, &caller, &[], &[], &[]);
        assert_eq!(
            codex.removed,
            ["CODEX_API_KEY", "OPENAI_API_KEY", "OPENAI_BASE_URL"]
        );
        assert_eq!(var(&codex, "OPENAI_ORG"), Some("d"));
        assert_eq!(var(&codex, "ANTHROPIC_API_KEY"), Some("e"));
        let pi = Session::assemble(Runtime::Pi, &caller, &[], &[], &[]);
        assert!(pi.removed.is_empty());
        for name in ["CODEX_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"] {
            assert!(var(&pi, name).is_some(), "{name}");
        }
    }

    #[test]
    fn later_sources_override_earlier_ones() {
        let caller = pairs(&[("A", "caller"), ("B", "caller"), ("C", "caller")]);
        let files = [
            file_entries(&[("B", "file1"), ("D", "file1")]),
            file_entries(&[("D", "file2"), ("E", "file2")]),
        ];
        let session = Session::assemble(
            Runtime::Pi,
            &caller,
            &[],
            &files,
            &arg_entries(&[("C", "arg"), ("E", "arg"), ("B", "arg2")]),
        );
        assert_eq!(var(&session, "A"), Some("caller"));
        assert_eq!(var(&session, "B"), Some("arg2"));
        assert_eq!(var(&session, "C"), Some("arg"));
        assert_eq!(var(&session, "D"), Some("file2"));
        assert_eq!(var(&session, "E"), Some("arg"));
        assert_eq!(session.set, ["B", "D", "E", "C"]);
    }

    #[test]
    fn agentrun_variables_are_read_from_every_source_and_removed() {
        let caller = pairs(&[("AGENTRUN_SANDBOX", "caller"), ("AGENTRUN_OTHER", "o")]);
        let files = [file_entries(&[("AGENTRUN_CODEX_AUTH", "codex-file")])];
        let session = Session::assemble(
            Runtime::Codex,
            &caller,
            &[],
            &files,
            &arg_entries(&[("AGENTRUN_PI_AUTH", "pi-arg"), ("KEEP", "1")]),
        );
        assert_eq!(session.sandbox.as_deref(), Some(OsStr::new("caller")));
        assert_eq!(
            session.codex_auth.as_deref(),
            Some(OsStr::new("codex-file"))
        );
        assert_eq!(session.pi_auth.as_deref(), Some(OsStr::new("pi-arg")));
        assert_eq!(
            session.agentrun_variables,
            [
                "AGENTRUN_CODEX_AUTH",
                "AGENTRUN_OTHER",
                "AGENTRUN_PI_AUTH",
                "AGENTRUN_SANDBOX"
            ]
        );
        assert!(
            session
                .env
                .keys()
                .all(|name| !name.as_bytes().starts_with(AGENTRUN_PREFIX))
        );
        assert_eq!(session.set, ["KEEP"]);
    }

    #[test]
    fn env_arg_overrides_agentrun_variable_from_file() {
        let files = [file_entries(&[("AGENTRUN_SANDBOX", "file")])];
        let session = Session::assemble(
            Runtime::Pi,
            &[],
            &[],
            &files,
            &arg_entries(&[("AGENTRUN_SANDBOX", "arg")]),
        );
        assert_eq!(session.sandbox.as_deref(), Some(OsStr::new("arg")));
        assert!(session.set.is_empty());
    }

    #[test]
    fn session_path_puts_path_dirs_first() {
        let caller = pairs(&[("PATH", "/usr/bin:/bin")]);
        let dirs = [PathBuf::from("/opt/b"), PathBuf::from("/opt/a")];
        let session = Session::assemble(Runtime::Pi, &caller, &dirs, &[], &[]);
        assert_eq!(session.path, "/opt/b:/opt/a:/usr/bin:/bin");
        assert_eq!(var(&session, "PATH"), Some("/opt/b:/opt/a:/usr/bin:/bin"));
        let overridden = Session::assemble(
            Runtime::Pi,
            &caller,
            &dirs,
            &[],
            &arg_entries(&[("PATH", "/custom")]),
        );
        assert_eq!(overridden.path, "/opt/b:/opt/a:/custom");
    }

    #[test]
    fn session_path_with_empty_or_missing_path() {
        let dirs = [PathBuf::from("/opt/a")];
        let empty = pairs(&[("PATH", "")]);
        assert_eq!(
            Session::assemble(Runtime::Pi, &empty, &dirs, &[], &[]).path,
            "/opt/a"
        );
        assert_eq!(
            Session::assemble(Runtime::Pi, &[], &dirs, &[], &[]).path,
            "/opt/a"
        );
        let session = Session::assemble(Runtime::Pi, &[], &[], &[], &[]);
        assert_eq!(session.path, "");
        assert_eq!(var(&session, "PATH"), Some(""));
    }

    #[test]
    fn path_dirs_become_absolute_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let resolved =
            resolve_path_dirs(&[second.clone(), link.clone(), PathBuf::from("src")]).unwrap();
        assert_eq!(resolved, vec![second, link, cwd.join("src")]);
    }

    #[test]
    fn path_dir_must_exist_and_be_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        for bad in [PathBuf::from("/no/such/dir"), file] {
            assert_eq!(
                resolve_path_dirs(std::slice::from_ref(&bad)),
                Err(format!("--path {}: not a directory", bad.display()))
            );
        }
    }

    #[test]
    fn find_executable_skips_non_executables_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        let folder = dir.path().join("folder");
        let good = dir.path().join("good");
        for sub in [&plain, &folder, &good] {
            std::fs::create_dir(sub).unwrap();
        }
        std::fs::write(plain.join("tool"), "").unwrap();
        std::fs::create_dir(folder.join("tool")).unwrap();
        executable(&good.join("tool"));
        let path = std::env::join_paths([Path::new(""), &plain, &folder, &good]).unwrap();
        assert_eq!(
            find_executable("tool", &path, Path::new("/")),
            Some(good.join("tool"))
        );
        assert_eq!(find_executable("missing", &path, Path::new("/")), None);
        assert_eq!(
            find_executable("tool", OsStr::new(""), Path::new("/")),
            None
        );
    }

    #[test]
    fn find_executable_keeps_symlinks_and_resolves_relative_entries_from_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let work = dir.path().join("work");
        let bin = work.join("bin");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        executable(&real.join("tool"));
        std::os::unix::fs::symlink(real.join("tool"), bin.join("tool")).unwrap();
        assert_eq!(
            find_executable("tool", OsStr::new("bin"), &work),
            Some(bin.join("tool"))
        );
        assert_eq!(
            find_executable("tool", OsStr::new("/nowhere:./bin"), &work),
            Some(work.join("./bin/tool"))
        );
    }
}
