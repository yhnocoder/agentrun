#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::json;
use support::env::Env;

fn with_claude(script: &str) -> Env {
    let env = Env::new();
    env.install("claude", script);
    env
}

fn with_output(lines: &str) -> Env {
    with_claude(&format!("#!/bin/sh\ncat <<'EOF'\n{lines}\nEOF\n"))
}

fn read_env_dump(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn child_environment_follows_assembly_rules() {
    let root = tempfile::tempdir().unwrap();
    let dump = root.path().join("env.txt");
    let env = with_claude(&format!("#!/bin/sh\n/usr/bin/env > '{}'\n", dump.display()));
    let env_file = env.root().join("session.env");
    std::fs::write(
        &env_file,
        "FILE_VAR=from-file\nKEEP=file\nAGENTRUN_PI_AUTH=file-auth\n",
    )
    .unwrap();
    let bin = env.bin().to_string_lossy().into_owned();
    let outcome = env.run_in_process(
        "claude-code",
        &[
            ("ANTHROPIC_API_KEY", "caller-key"),
            ("CLAUDE_CODE_USE_VERTEX", "1"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "oauth"),
            ("KEEP", "caller"),
            ("INHERITED", "yes"),
            ("AGENTRUN_SANDBOX", "off"),
            ("AGENTRUN_UNKNOWN", "x"),
        ],
        &[
            "--env-file",
            env_file.to_str().unwrap(),
            "--env",
            "KEEP=arg",
            "--env",
            "ANTHROPIC_BASE_URL=https://example.test",
            "--env",
            "PATH=/usr/bin:/bin",
            "--env",
            "TMPDIR=/ignored",
            "--path",
            &bin,
        ],
        false,
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let child = read_env_dump(&dump);
    assert!(
        child.keys().all(|key| !key.starts_with("AGENTRUN_")),
        "{child:?}"
    );
    let session_path = format!("{bin}:/usr/bin:/bin");
    assert_eq!(child["PATH"], session_path);
    assert!(!child.contains_key("ANTHROPIC_API_KEY"));
    assert!(!child.contains_key("CLAUDE_CODE_USE_VERTEX"));
    assert_eq!(child["CLAUDE_CODE_OAUTH_TOKEN"], "oauth");
    assert_eq!(child["ANTHROPIC_BASE_URL"], "https://example.test");
    assert_eq!(child["KEEP"], "arg");
    assert_eq!(child["FILE_VAR"], "from-file");
    assert_eq!(child["INHERITED"], "yes");
    let tempdir = Path::new(&child["TMPDIR"]);
    assert_eq!(tempdir.parent().unwrap(), env.tmp());
    assert_eq!(child["TMP"], child["TMPDIR"]);
    assert_eq!(child["TEMP"], child["TMPDIR"]);
    let start = &outcome.events()[0];
    assert_eq!(start["type"], "start");
    assert_eq!(
        start["env"],
        json!(["FILE_VAR", "KEEP", "ANTHROPIC_BASE_URL", "PATH", "TMPDIR"])
    );
}

#[test]
fn pi_credential_is_written_and_never_printed() {
    let secret = "pi-credential-value-7f3a9c";
    let env = with_claude("#!/bin/sh\nexit 0\n");
    env.install(
        "pi",
        "#!/bin/sh\n/usr/bin/env\n/usr/bin/env >&2\nprintf '{\"record\":\"text\",\"parent\":null,\"text\":\"done\"}\\n'\n",
    );
    let home = env.home();
    let home_str = home.to_string_lossy().into_owned();
    let raw = env.root().join("raw.jsonl");
    let caller_env = [("HOME", home_str.as_str()), ("AGENTRUN_PI_AUTH", secret)];
    let args = ["--debug", "--raw", raw.to_str().unwrap()];
    let outcome = env.run_in_process("pi", &caller_env, &args, false);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let login = home.join(".pi/agent/auth.json");
    assert_eq!(std::fs::read_to_string(&login).unwrap(), secret);
    assert!(home.join(".pi/agent/auth.json.agentrun-sha256").exists());
    assert!(outcome.stderr.contains(&format!(
        "[debug] credentials: AGENTRUN_PI_AUTH -> {} (written)\n",
        login.display()
    )));
    assert!(
        outcome
            .stderr
            .contains("[debug] agentrun variables: AGENTRUN_PI_AUTH\n")
    );
    assert!(outcome.stderr.contains(&format!("HOME={home_str}\n")));
    let raw_text = std::fs::read_to_string(&raw).unwrap();
    assert!(raw_text.contains(&format!("HOME={home_str}\n")));
    for text in [&outcome.stdout, &outcome.stderr, &raw_text] {
        assert!(!text.contains(secret));
    }
    std::fs::write(&login, "refreshed").unwrap();
    let again = env.run_in_process("pi", &caller_env, &args, false);
    assert!(again.stderr.contains(&format!(
        "[debug] credentials: AGENTRUN_PI_AUTH -> {} (unchanged)\n",
        login.display()
    )));
    assert_eq!(std::fs::read_to_string(&login).unwrap(), "refreshed");
}

#[test]
fn dry_run_lists_variable_names_and_skips_credentials() {
    let env = with_claude("#!/bin/sh\nexit 0\n");
    env.install("pi", "#!/bin/sh\ntouch \"$0.ran\"\n");
    let home = env.home();
    let home_str = home.to_string_lossy().into_owned();
    let env_file = env.root().join("a.env");
    std::fs::write(&env_file, "AGENTRUN_SANDBOX=relax\nZED=1\nALPHA=2\n").unwrap();
    let outcome = env.run_in_process(
        "pi",
        &[
            ("HOME", home_str.as_str()),
            ("AGENTRUN_PI_AUTH", "secret"),
            ("GH_TOKEN", "token"),
        ],
        &[
            "--dry-run",
            "--env-file",
            env_file.to_str().unwrap(),
            "--env",
            "GH_TOKEN",
            "--env",
            "ZED=3",
        ],
        false,
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        format!(
            "command: {} '<prompt 2 bytes>'\nPATH: {}\nset: ZED, ALPHA, GH_TOKEN\nremoved: (none)\nagentrun variables: AGENTRUN_PI_AUTH, AGENTRUN_SANDBOX\n",
            env.bin().join("pi").display(),
            env.path_var()
        )
    );
    assert!(outcome.stderr.is_empty());
    assert!(!env.home().join(".pi").exists());
    assert!(!env.bin().join("pi.ran").exists());
    assert!(env.leftovers().is_empty());
}

#[test]
fn debug_lists_removed_variables_for_claude_code() {
    let env = with_output("");
    let outcome = env.run_in_process(
        "claude-code",
        &[
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("ANTHROPIC_API_KEY", "key"),
            ("AGENTRUN_CODEX_AUTH", "codex"),
        ],
        &["--debug", "--env", "ANTHROPIC_API_KEY"],
        false,
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let lines: Vec<&str> = outcome.stderr.lines().collect();
    assert_eq!(
        lines[3..7],
        [
            "[debug] set: ANTHROPIC_API_KEY",
            "[debug] removed: ANTHROPIC_API_KEY, CLAUDE_CODE_USE_BEDROCK",
            "[debug] agentrun variables: AGENTRUN_CODEX_AUTH",
            "[debug] credentials: (none)",
        ]
    );
}

#[test]
fn credential_write_failure_is_rejected_before_tempdir() {
    let env = with_claude("#!/bin/sh\nexit 0\n");
    env.install("pi", "#!/bin/sh\ntouch \"$0.ran\"\n");
    let blocker = env.root().join("blocker");
    std::fs::write(&blocker, "").unwrap();
    let agent_dir = blocker.join("agent");
    let outcome = env.run_in_process(
        "pi",
        &[
            ("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap()),
            ("AGENTRUN_PI_AUTH", "secret-value"),
        ],
        &[],
        false,
    );
    assert_eq!(outcome.code, 2);
    let detail = outcome.end()["detail"].as_str().unwrap().to_string();
    assert!(
        detail.starts_with(&format!(
            "cannot write AGENTRUN_PI_AUTH to {}: ",
            agent_dir.join("auth.json").display()
        )),
        "{detail}"
    );
    assert!(!outcome.stdout.contains("secret-value"));
    assert!(!outcome.stderr.contains("secret-value"));
    assert!(!env.bin().join("pi.ran").exists());
    assert!(env.leftovers().is_empty());
}
