use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

struct Env {
    root: TempDir,
}

impl Env {
    fn new() -> Env {
        let env = Env {
            root: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir(env.bin()).unwrap();
        std::fs::create_dir(env.tmp()).unwrap();
        env
    }

    fn with_fake_claude() -> Env {
        let env = Env::new();
        env.install("claude", "#!/bin/sh\nexit 0\n");
        env
    }

    fn install(&self, name: &str, script: &str) {
        let path = self.bin().join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    fn tmp(&self) -> PathBuf {
        self.root.path().join("tmp")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentrun"));
        command
            .args(args)
            .current_dir(self.root.path())
            .env_clear()
            .env("PATH", self.bin())
            .env("TMPDIR", self.tmp())
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn run_with_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn no_leftover_tempdirs(&self) -> bool {
        std::fs::read_dir(self.tmp()).unwrap().next().is_none()
    }
}

fn assert_rejected(output: &Output) -> String {
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "{stdout}");
    let end: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(end["schema"], 1);
    assert_eq!(end["type"], "end");
    assert_eq!(end["status"], "rejected");
    assert_eq!(end["exit_code"], Value::Null);
    assert_eq!(end["result"], Value::Null);
    assert_eq!(
        end["usage"],
        serde_json::json!({"input_tokens": null, "output_tokens": null, "cache_read_tokens": null, "cache_write_tokens": null, "by_model": {}})
    );
    let detail = end["detail"].as_str().unwrap().to_string();
    assert!(!detail.is_empty());
    assert!(!detail.contains('\n'));
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert_eq!(stderr, format!("agentrun: {detail}\n"));
    detail
}

#[test]
fn unknown_option_is_rejected() {
    let detail = assert_rejected(&Env::new().run(&["pi", "--bogus", "--prompt", "hi"]));
    assert!(detail.contains("--bogus"), "{detail}");
    assert!(!detail.contains("Usage:"));
    assert!(!detail.contains("For more information"));
}

#[test]
fn unknown_subcommand_is_rejected() {
    let detail = assert_rejected(&Env::new().run(&["doctor"]));
    assert!(detail.contains("doctor"), "{detail}");
}

#[test]
fn prompt_and_prompt_file_together_are_rejected() {
    let env = Env::new();
    let file = env.root.path().join("task.md");
    std::fs::write(&file, "task").unwrap();
    assert_rejected(&env.run(&[
        "pi",
        "--prompt",
        "hi",
        "--prompt-file",
        file.to_str().unwrap(),
    ]));
}

#[test]
fn empty_and_blank_prompts_are_rejected() {
    let env = Env::new();
    assert_eq!(
        assert_rejected(&env.run(&["pi", "--prompt", ""])),
        "the prompt is empty"
    );
    assert_eq!(
        assert_rejected(&env.run(&["pi", "--prompt", " \n\t"])),
        "the prompt is empty"
    );
    assert_eq!(
        assert_rejected(&env.run_with_stdin(&["pi"], "  \n")),
        "the prompt is empty"
    );
}

#[test]
fn missing_prompt_file_is_rejected() {
    let env = Env::new();
    let file = env.root.path().join("missing.md");
    let detail = assert_rejected(&env.run(&["pi", "--prompt-file", file.to_str().unwrap()]));
    assert!(
        detail.starts_with(&format!("cannot read prompt file {}: ", file.display())),
        "{detail}"
    );
}

#[test]
fn non_utf8_prompt_file_is_rejected() {
    let env = Env::new();
    let file = env.root.path().join("binary.md");
    std::fs::write(&file, [0xff, 0xfe]).unwrap();
    let detail = assert_rejected(&env.run(&["pi", "--prompt-file", file.to_str().unwrap()]));
    assert_eq!(
        detail,
        format!("prompt file {} is not valid UTF-8", file.display())
    );
}

#[test]
fn missing_cwd_is_rejected() {
    let env = Env::new();
    let dir = env.root.path().join("nowhere");
    let detail =
        assert_rejected(&env.run(&["pi", "--cwd", dir.to_str().unwrap(), "--prompt", "hi"]));
    assert!(detail.contains(dir.to_str().unwrap()), "{detail}");
}

#[test]
fn max_turns_is_only_for_claude_code() {
    let env = Env::new();
    assert_eq!(
        assert_rejected(&env.run(&["pi", "--max-turns", "3", "--prompt", "hi"])),
        "--max-turns is only supported by claude-code"
    );
    assert_rejected(&env.run(&["claude-code", "--max-turns", "0", "--prompt", "hi"]));
}

#[test]
fn invalid_sandbox_value_is_rejected() {
    let detail = assert_rejected(&Env::new().run(&["pi", "--sandbox", "maybe", "--prompt", "hi"]));
    assert!(detail.contains("maybe"), "{detail}");
}

#[test]
fn raw_file_that_cannot_be_created_is_rejected() {
    let env = Env::new();
    let raw = env.root.path().join("missing-dir/raw.jsonl");
    let detail =
        assert_rejected(&env.run(&["pi", "--prompt", "hi", "--raw", raw.to_str().unwrap()]));
    assert!(
        detail.starts_with(&format!("cannot create raw file {}: ", raw.display())),
        "{detail}"
    );
}

#[test]
fn text_format_rejection_is_one_end_line() {
    let env = Env::new();
    let cases: [&[&str]; 2] = [
        &["pi", "--format", "text", "--prompt", ""],
        &["pi", "--format=text", "--prompt", ""],
    ];
    for args in cases {
        let output = env.run(args);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "[end] rejected the prompt is empty\n"
        );
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "agentrun: the prompt is empty\n"
        );
    }
}

#[test]
fn prescan_applies_when_parsing_fails() {
    let env = Env::new();
    let cases: [&[&str]; 2] = [
        &["pi", "--format", "text", "--bogus"],
        &["pi", "--bogus", "--format=text"],
    ];
    for args in cases {
        let output = env.run(args);
        assert_eq!(output.status.code(), Some(2));
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.starts_with("[end] rejected "), "{stdout}");
        assert!(stdout.contains("--bogus"));
        assert_eq!(stdout.lines().count(), 1);
    }
}

#[test]
fn prompt_from_stdin_passes_prompt_checks() {
    let env = Env::new();
    let detail = assert_rejected(&env.run_with_stdin(&["claude-code"], "fix the tests\n"));
    assert_eq!(detail, "claude not found in PATH");
}

#[test]
fn runtime_missing_from_path_is_rejected() {
    let env = Env::new();
    assert_eq!(
        assert_rejected(&env.run(&["claude-code", "--prompt", "hi"])),
        "claude not found in PATH"
    );
    assert_eq!(
        assert_rejected(&env.run(&["pi", "--prompt", "hi"])),
        "pi not found in PATH"
    );
}

const INSTALL_HINT: &str = "Install bubblewrap and socat (for example: apt-get install bubblewrap socat, or dnf install bubblewrap socat), or use --sandbox relax or --sandbox off";
const ADAPTER_MISSING: &str = "claude-code support is not implemented in this build";

fn assert_rejected_with_debug(output: &Output, debug_lines: &[&str]) -> String {
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let end: Value = serde_json::from_str(stdout.trim_end()).unwrap();
    assert_eq!(end["status"], "rejected");
    let detail = end["detail"].as_str().unwrap().to_string();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let mut expected: Vec<String> = debug_lines
        .iter()
        .map(|line| format!("[debug] {line}\n"))
        .collect();
    expected.push(format!("agentrun: {detail}\n"));
    assert_eq!(stderr, expected.concat());
    detail
}

#[test]
fn sandbox_on_without_bwrap_is_rejected_without_tempdir() {
    let env = Env::with_fake_claude();
    assert_eq!(
        assert_rejected(&env.run(&["claude-code", "--prompt", "hi"])),
        format!("sandbox is not available: bwrap not found in PATH. {INSTALL_HINT}")
    );
    assert_eq!(
        assert_rejected(&env.run(&["claude-code", "--dry-run", "--prompt", "hi"])),
        format!("sandbox is not available: bwrap not found in PATH. {INSTALL_HINT}")
    );
    assert!(env.no_leftover_tempdirs());
}

#[test]
fn sandbox_relax_without_bwrap_passes_the_sandbox_step() {
    let env = Env::with_fake_claude();
    let output = env.run(&[
        "claude-code",
        "--sandbox",
        "relax",
        "--debug",
        "--prompt",
        "hi",
    ]);
    assert_eq!(
        assert_rejected_with_debug(
            &output,
            &["sandbox: none (--sandbox relax: bwrap not found in PATH)"]
        ),
        ADAPTER_MISSING
    );
}

#[test]
fn bwrap_that_cannot_start_is_rejected_with_its_message() {
    let env = Env::with_fake_claude();
    env.install(
        "bwrap",
        "#!/bin/sh\necho 'bwrap: No permissions to create new namespace' >&2\nexit 1\n",
    );
    env.install("socat", "#!/bin/sh\nexit 0\n");
    assert_eq!(
        assert_rejected(&env.run(&["claude-code", "--prompt", "hi"])),
        "sandbox is not available: bwrap cannot start: No permissions to create new namespace. bwrap cannot create a sandbox here. In a docker container use --sandbox off"
    );
    env.install("bwrap", "#!/bin/sh\nexit 3\n");
    assert_eq!(
        assert_rejected(&env.run(&["claude-code", "--prompt", "hi"])),
        "sandbox is not available: bwrap cannot start: exited with code 3. bwrap cannot create a sandbox here. In a docker container use --sandbox off"
    );
}

#[test]
fn bwrap_that_hangs_is_killed_after_five_seconds() {
    let env = Env::with_fake_claude();
    env.install("bwrap", "#!/bin/sh\nexec /bin/sleep 10\n");
    env.install("socat", "#!/bin/sh\nexit 0\n");
    let started = std::time::Instant::now();
    let detail = assert_rejected(&env.run(&["claude-code", "--prompt", "hi"]));
    let elapsed = started.elapsed().as_secs_f64();
    assert!((4.5..9.0).contains(&elapsed), "took {elapsed:.2}s");
    assert_eq!(
        detail,
        "sandbox is not available: bwrap cannot start: timed out after 5 seconds. bwrap cannot create a sandbox here. In a docker container use --sandbox off"
    );
}

#[test]
fn missing_socat_is_rejected() {
    let env = Env::with_fake_claude();
    env.install("bwrap", "#!/bin/sh\nexit 0\n");
    assert_eq!(
        assert_rejected(&env.run(&["claude-code", "--prompt", "hi"])),
        format!("sandbox is not available: socat not found in PATH. {INSTALL_HINT}")
    );
}

#[test]
fn sandbox_variable_from_every_source_and_option_precedence() {
    let env = Env::with_fake_claude();
    let output = env
        .command(&["claude-code", "--prompt", "hi"])
        .env("AGENTRUN_SANDBOX", "off")
        .output()
        .unwrap();
    assert_eq!(assert_rejected(&output), ADAPTER_MISSING);
    let output = env
        .command(&["claude-code", "--env", "AGENTRUN_SANDBOX", "--prompt", "hi"])
        .env("AGENTRUN_SANDBOX", "off")
        .output()
        .unwrap();
    assert_eq!(assert_rejected(&output), ADAPTER_MISSING);
    assert_eq!(
        assert_rejected(&env.run(&[
            "claude-code",
            "--env",
            "AGENTRUN_SANDBOX=off",
            "--prompt",
            "hi"
        ])),
        ADAPTER_MISSING
    );
    std::fs::write(
        env.root.path().join("sandbox.env"),
        "AGENTRUN_SANDBOX=off\n",
    )
    .unwrap();
    assert_eq!(
        assert_rejected(&env.run(&["claude-code", "--env-file", "sandbox.env", "--prompt", "hi"])),
        ADAPTER_MISSING
    );
    assert_eq!(
        assert_rejected(&env.run(&[
            "claude-code",
            "--env-file",
            "sandbox.env",
            "--sandbox",
            "on",
            "--prompt",
            "hi"
        ])),
        format!("sandbox is not available: bwrap not found in PATH. {INSTALL_HINT}")
    );
    let output = env
        .command(&["claude-code", "--prompt", "hi"])
        .env("AGENTRUN_SANDBOX", "maybe")
        .output()
        .unwrap();
    assert_eq!(
        assert_rejected(&output),
        "invalid AGENTRUN_SANDBOX value 'maybe': expected on, relax or off"
    );
    assert!(env.no_leftover_tempdirs());
}

#[test]
fn network_full_and_custom_are_not_implemented_yet() {
    let env = Env::with_fake_claude();
    assert_eq!(
        assert_rejected(&env.run(&[
            "claude-code",
            "--sandbox",
            "off",
            "--network",
            "custom",
            "--prompt",
            "hi"
        ])),
        "--network custom is not implemented in this build. Use --network none"
    );
    assert_eq!(
        assert_rejected(&env.run(&[
            "claude-code",
            "--sandbox",
            "relax",
            "--network",
            "full",
            "--prompt",
            "hi"
        ])),
        "--network full is not implemented in this build. Use --network none"
    );
}

#[test]
fn real_bwrap_passes_the_sandbox_step() {
    let Some(bwrap) = ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
    else {
        eprintln!("skipped: bwrap is not installed");
        return;
    };
    let env = Env::with_fake_claude();
    let system = bwrap.parent().unwrap();
    let output = env
        .command(&["claude-code", "--debug", "--prompt", "hi"])
        .env(
            "PATH",
            format!("{}:{}", env.bin().display(), system.display()),
        )
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let first = stderr.lines().next().unwrap_or_default();
    if first.contains("bwrap cannot start") {
        eprintln!("skipped: {first}");
        return;
    }
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        first.starts_with(&format!(
            "[debug] sandbox: bubblewrap ({}, socat {}/socat, check ",
            bwrap.display(),
            system.display()
        )),
        "{first}"
    );
    assert_eq!(
        stderr.lines().nth(1),
        Some(format!("agentrun: {ADAPTER_MISSING}").as_str())
    );
    let output = env
        .command(&["claude-code", "--network", "full", "--prompt", "hi"])
        .env(
            "PATH",
            format!("{}:{}", env.bin().display(), system.display()),
        )
        .output()
        .unwrap();
    assert_eq!(
        assert_rejected(&output),
        "--network full is not implemented in this build. Use --network none"
    );
}

#[test]
fn missing_adapter_is_rejected_without_tempdir() {
    let env = Env::with_fake_claude();
    assert_eq!(
        assert_rejected(&env.run(&["claude-code", "--sandbox", "off", "--prompt", "hi"])),
        "claude-code support is not implemented in this build"
    );
    assert!(env.no_leftover_tempdirs());
}

#[test]
fn env_without_caller_value_is_rejected() {
    let env = Env::new();
    assert_eq!(
        assert_rejected(&env.run(&["pi", "--env", "NO_SUCH_VAR", "--prompt", "hi"])),
        "--env NO_SUCH_VAR: not set in the caller's environment"
    );
    assert!(env.no_leftover_tempdirs());
}

#[test]
fn env_with_invalid_name_is_rejected() {
    assert_eq!(
        assert_rejected(&Env::new().run(&["pi", "--env", "1BAD=x", "--prompt", "hi"])),
        "--env: invalid variable name '1BAD'"
    );
}

#[test]
fn missing_path_dir_is_rejected() {
    assert_eq!(
        assert_rejected(&Env::new().run(&["pi", "--path", "/no/such/dir", "--prompt", "hi"])),
        "--path /no/such/dir: not a directory"
    );
}

#[test]
fn missing_env_file_is_rejected() {
    let detail =
        assert_rejected(&Env::new().run(&["pi", "--env-file", "missing.env", "--prompt", "hi"]));
    assert!(detail.starts_with("--env-file missing.env: "), "{detail}");
}

#[test]
fn malformed_env_file_line_is_rejected_without_its_content() {
    let env = Env::new();
    std::fs::write(
        env.root.path().join("bad.env"),
        "A=1\n# comment\nSECRET_NAME = hunter2\n",
    )
    .unwrap();
    let detail = assert_rejected(&env.run(&["pi", "--env-file", "bad.env", "--prompt", "hi"]));
    assert_eq!(detail, "--env-file bad.env: line 3: expected KEY=VALUE");
    assert!(!detail.contains("hunter2") && !detail.contains("SECRET_NAME"));
}

#[test]
fn runtime_is_found_through_path_option() {
    let env = Env::with_fake_claude();
    let empty = env.root.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let output = env
        .command(&[
            "claude-code",
            "--sandbox",
            "off",
            "--path",
            env.bin().to_str().unwrap(),
            "--prompt",
            "hi",
        ])
        .env("PATH", &empty)
        .output()
        .unwrap();
    assert_eq!(
        assert_rejected(&output),
        "claude-code support is not implemented in this build"
    );
    let output = env
        .command(&["claude-code", "--sandbox", "off", "--prompt", "hi"])
        .env("PATH", &empty)
        .output()
        .unwrap();
    assert_eq!(assert_rejected(&output), "claude not found in PATH");
}

#[test]
fn help_and_version_print_to_stdout() {
    let env = Env::new();
    for args in [["--help"], ["--version"]] {
        let output = env.run(&args);
        assert_eq!(output.status.code(), Some(0));
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert!(
            !String::from_utf8(output.stdout)
                .unwrap()
                .contains("\"type\"")
        );
    }
    let output = env.run(&["claude-code", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("--max-turns")
    );
}
