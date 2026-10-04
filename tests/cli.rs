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
        let path = env.bin().join("claude");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        env
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

#[test]
fn sandbox_on_is_rejected_without_tempdir() {
    let env = Env::with_fake_claude();
    assert_eq!(
        assert_rejected(&env.run(&["claude-code", "--prompt", "hi"])),
        "sandbox is not available: not implemented in this build. Use --sandbox relax or --sandbox off"
    );
    assert!(env.no_leftover_tempdirs());
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
