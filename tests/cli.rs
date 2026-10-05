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

    fn with_fake_pi() -> Env {
        let env = Env::new();
        env.install("pi", "#!/bin/sh\nexit 0\n");
        env
    }

    fn with_fake_codex() -> Env {
        let env = Env::new();
        env.install("codex", "#!/bin/sh\nexit 0\n");
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

#[cfg(target_os = "linux")]
const INSTALL_HINT: &str = "Install bubblewrap and socat (for example: apt-get install bubblewrap socat, or dnf install bubblewrap socat), or use --sandbox relax or --sandbox off";
const CANNOT_START_HINT: &str = "In a docker container use --sandbox off. On Ubuntu 23.10 or later, allow bwrap to create user namespaces with an AppArmor profile: https://yhnocoder.github.io/agentrun/pages/isolation.html#apparmor";
const ADAPTER_MISSING: &str = "codex support is not implemented in this build";

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
#[test]
fn sandbox_relax_without_bwrap_passes_the_sandbox_step() {
    let env = Env::with_fake_pi();
    let output = env.run(&[
        "pi",
        "--sandbox",
        "relax",
        "--debug",
        "--dry-run",
        "--prompt",
        "hi",
    ]);
    assert_dry_run(&output);
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "[debug] sandbox: none (--sandbox relax: bwrap not found in PATH)\n"
    );
}

#[cfg(target_os = "linux")]
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
        format!(
            "sandbox is not available: bwrap cannot start: No permissions to create new namespace. {CANNOT_START_HINT}"
        )
    );
    env.install("bwrap", "#!/bin/sh\nexit 3\n");
    assert_eq!(
        assert_rejected(&env.run(&["claude-code", "--prompt", "hi"])),
        format!(
            "sandbox is not available: bwrap cannot start: exited with code 3. {CANNOT_START_HINT}"
        )
    );
}

#[test]
fn codex_is_rejected_when_bwrap_cannot_start_and_needs_no_socat() {
    let env = Env::with_fake_codex();
    let work = env.root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let args = ["codex", "--cwd", work.to_str().unwrap(), "--prompt", "hi"];
    env.install(
        "bwrap",
        "#!/bin/sh\necho 'bwrap: setting up uid map: Permission denied' >&2\nexit 1\n",
    );
    assert_eq!(
        assert_rejected(&env.run(&args)),
        format!(
            "sandbox is not available: bwrap cannot start: setting up uid map: Permission denied. {CANNOT_START_HINT}"
        )
    );
    env.install("bwrap", "#!/bin/sh\nexit 0\n");
    assert_eq!(assert_rejected(&env.run(&args)), ADAPTER_MISSING);
    assert_eq!(
        assert_rejected(&env.run(&["codex", "--prompt", "hi"])),
        "sandbox is not available: bwrap not found in PATH. Install bubblewrap (for example: apt-get install bubblewrap, or dnf install bubblewrap), or use --sandbox relax or --sandbox off"
    );
}

#[cfg(target_os = "linux")]
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
        format!(
            "sandbox is not available: bwrap cannot start: timed out after 5 seconds. {CANNOT_START_HINT}"
        )
    );
}

#[cfg(target_os = "linux")]
#[test]
fn missing_socat_is_rejected() {
    let env = Env::with_fake_claude();
    env.install("bwrap", "#!/bin/sh\nexit 0\n");
    assert_eq!(
        assert_rejected(&env.run(&["claude-code", "--prompt", "hi"])),
        format!("sandbox is not available: socat not found in PATH. {INSTALL_HINT}")
    );
}

fn assert_dry_run(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    assert!(stdout.starts_with("command: "), "{stdout}");
}

#[cfg(target_os = "linux")]
const SANDBOX_ON_WITHOUT_PROVIDER: &str = "sandbox is not available: bwrap not found in PATH. Install bubblewrap and socat (for example: apt-get install bubblewrap socat, or dnf install bubblewrap socat), or use --sandbox relax or --sandbox off";

#[cfg(target_os = "macos")]
const SANDBOX_ON_WITHOUT_PROVIDER: &str = "cannot tell which host pi's model service uses (provider: unknown). Use --network custom --allow-host <host of the model service>";

#[test]
fn sandbox_variable_from_every_source_and_option_precedence() {
    let env = Env::with_fake_pi();
    let output = env
        .command(&["pi", "--dry-run", "--prompt", "hi"])
        .env("AGENTRUN_SANDBOX", "off")
        .output()
        .unwrap();
    assert_dry_run(&output);
    let output = env
        .command(&[
            "pi",
            "--dry-run",
            "--env",
            "AGENTRUN_SANDBOX",
            "--prompt",
            "hi",
        ])
        .env("AGENTRUN_SANDBOX", "off")
        .output()
        .unwrap();
    assert_dry_run(&output);
    assert_dry_run(&env.run(&[
        "pi",
        "--dry-run",
        "--env",
        "AGENTRUN_SANDBOX=off",
        "--prompt",
        "hi",
    ]));
    std::fs::write(
        env.root.path().join("sandbox.env"),
        "AGENTRUN_SANDBOX=off\n",
    )
    .unwrap();
    assert_dry_run(&env.run(&[
        "pi",
        "--dry-run",
        "--env-file",
        "sandbox.env",
        "--prompt",
        "hi",
    ]));
    assert_eq!(
        assert_rejected(&env.run(&[
            "pi",
            "--dry-run",
            "--env-file",
            "sandbox.env",
            "--sandbox",
            "on",
            "--prompt",
            "hi"
        ])),
        SANDBOX_ON_WITHOUT_PROVIDER
    );
    let output = env
        .command(&["pi", "--prompt", "hi"])
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
fn network_usage_errors() {
    let env = Env::with_fake_pi();
    let cases = [
        (
            vec!["--network", "custom"],
            "--network custom requires at least one --allow-host".to_string(),
        ),
        (
            vec!["--allow-host", "example.com"],
            "--allow-host requires --network custom".to_string(),
        ),
        (
            vec!["--network", "full", "--allow-host", "example.com"],
            "--allow-host requires --network custom".to_string(),
        ),
        (
            vec!["--network", "custom", "--allow-host", "*"],
            "--allow-host '*' is not allowed. Use --network full".to_string(),
        ),
        (
            vec!["--network", "custom", "--allow-host", "api.*.com"],
            "--allow-host 'api.*.com': a wildcard is only allowed as the first label, as in *.example.com".to_string(),
        ),
        (
            vec!["--network", "custom", "--allow-host", "*.*.com"],
            "--allow-host '*.*.com': a wildcard is only allowed as the first label, as in *.example.com".to_string(),
        ),
        (
            vec!["--network", "custom", "--allow-host", ":443"],
            "--allow-host ':443': expected HOST or HOST:PORT".to_string(),
        ),
        (
            vec!["--network", "custom", "--allow-host", "example.com:0"],
            "--allow-host 'example.com:0': expected HOST or HOST:PORT".to_string(),
        ),
        (
            vec!["--network", "custom", "--allow-host", "example.com:70000"],
            "--allow-host 'example.com:70000': expected HOST or HOST:PORT".to_string(),
        ),
        (
            vec!["--network", "custom", "--allow-host", "[::1]:443"],
            "--allow-host '[::1]:443': IPv6 addresses are not supported".to_string(),
        ),
        (
            vec!["--network", "custom", "--allow-host", "::1"],
            "--allow-host '::1': IPv6 addresses are not supported".to_string(),
        ),
    ];
    for (options, detail) in cases {
        let mut args = vec!["pi", "--sandbox", "off"];
        args.extend(options);
        args.extend(["--prompt", "hi"]);
        assert_eq!(assert_rejected(&env.run(&args)), detail, "{args:?}");
    }
    assert!(env.no_leftover_tempdirs());
}

#[test]
fn start_network_reports_mode_allow_and_enforcement() {
    let env = Env::new();
    env.install(
        "pi",
        "#!/bin/sh\necho '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"provider\":\"deepseek\",\"model\":\"deepseek-flash\",\"usage\":{\"input\":1,\"output\":1,\"cacheRead\":0,\"cacheWrite\":0},\"stopReason\":\"stop\"}}'\n",
    );
    let output = env.run(&[
        "pi",
        "--sandbox",
        "off",
        "--network",
        "custom",
        "--allow-host",
        "API.Example.com.",
        "--allow-host",
        "*.github.com:443",
        "--prompt",
        "hi",
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let start: Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(start["type"], "start");
    assert_eq!(
        start["network"],
        serde_json::json!({
            "mode": "custom",
            "allow": ["API.Example.com.", "*.github.com:443"],
            "enforced": false,
        })
    );
    let output = env.run(&[
        "pi",
        "--sandbox",
        "off",
        "--network",
        "full",
        "--prompt",
        "hi",
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let start: Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(
        start["network"],
        serde_json::json!({"mode": "full", "allow": [], "enforced": false})
    );
}

#[cfg(target_os = "linux")]
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
    let env = Env::with_fake_pi();
    let system = bwrap.parent().unwrap();
    let path = format!("{}:{}", env.bin().display(), system.display());
    let output = env
        .command(&[
            "pi",
            "--debug",
            "--dry-run",
            "--model",
            "deepseek/deepseek-flash",
            "--prompt",
            "hi",
        ])
        .env("PATH", &path)
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let first = stderr.lines().next().unwrap_or_default();
    if first.contains("bwrap cannot start") {
        eprintln!("skipped: {first}");
        return;
    }
    assert_dry_run(&output);
    assert!(
        first.starts_with(&format!(
            "[debug] sandbox: bubblewrap ({}, socat {}/socat, check ",
            bwrap.display(),
            system.display()
        )),
        "{first}"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let command = stdout.lines().next().unwrap();
    assert!(
        command.contains(&format!(
            " {}/socat '<proxy port>' '<tempdir>/proxy.sock' {}/pi ",
            system.display(),
            env.bin().display()
        )),
        "{command}"
    );
    assert!(command.contains("--unshare-net"), "{command}");
    assert!(env.no_leftover_tempdirs());
    let output = env
        .command(&["pi", "--dry-run", "--prompt", "hi"])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert_eq!(
        assert_rejected(&output),
        "cannot tell which host pi's model service uses (provider: unknown). Use --network custom --allow-host <host of the model service>"
    );
    let output = env
        .command(&["pi", "--dry-run", "--model", "acme/robot", "--prompt", "hi"])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert_eq!(
        assert_rejected(&output),
        "cannot tell which host pi's model service uses (provider: acme). Use --network custom --allow-host <host of the model service>"
    );
    for mode in ["custom", "full"] {
        let mut args = vec![
            "pi",
            "--dry-run",
            "--model",
            "acme/robot",
            "--network",
            mode,
        ];
        if mode == "custom" {
            args.extend(["--allow-host", "robot.example"]);
        }
        args.extend(["--prompt", "hi"]);
        let output = env.command(&args).env("PATH", &path).output().unwrap();
        assert_dry_run(&output);
    }
    assert!(env.no_leftover_tempdirs());
    env.install("claude", "#!/bin/sh\nexit 0\n");
    let output = env
        .command(&[
            "claude-code",
            "--network",
            "full",
            "--dry-run",
            "--prompt",
            "hi",
        ])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert_dry_run(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let command = stdout.lines().next().unwrap();
    assert!(
        command.contains(
            r#""network":{"allowedDomains":[],"httpProxyPort":"<proxy port>","socksProxyPort":"<proxy port>"}"#
        ),
        "{command}"
    );
    let output = env
        .command(&["claude-code", "--dry-run", "--prompt", "hi"])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert_dry_run(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let command = stdout.lines().next().unwrap();
    assert!(
        command.contains(r#""network":{"allowedDomains":[]}"#),
        "{command}"
    );
    assert!(!command.contains("proxy port"), "{command}");
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_exec_passes_the_sandbox_step() {
    let env = Env::with_fake_claude();
    env.install("pi", "#!/bin/sh\nexit 0\n");
    let output = env.run(&[
        "pi",
        "--debug",
        "--dry-run",
        "--model",
        "deepseek/deepseek-flash",
        "--prompt",
        "hi",
    ]);
    assert_dry_run(&output);
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let first = stderr.lines().next().unwrap_or_default();
    assert!(
        first.starts_with("[debug] sandbox: seatbelt (/usr/bin/sandbox-exec, check "),
        "{first}"
    );
    assert!(first.ends_with("ms)"), "{first}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let command = stdout.lines().next().unwrap();
    assert!(
        command.starts_with(&format!(
            "command: /usr/bin/sandbox-exec -f '<tempdir>/seatbelt.sb' {}/pi -p ",
            env.bin().display()
        )),
        "{command}"
    );
    assert!(!command.contains("socat"), "{command}");
    assert!(env.no_leftover_tempdirs());
    let output = env.run(&["pi", "--sandbox", "off", "--dry-run", "--prompt", "hi"]);
    assert_dry_run(&output);
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("sandbox-exec"),
        "the unsandboxed command is not wrapped"
    );
    let output = env.run(&["claude-code", "--dry-run", "--prompt", "hi"]);
    assert_dry_run(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(r#""sandbox":{"enabled":true"#), "{stdout}");
    assert!(!stdout.contains("sandbox-exec"), "{stdout}");
}

#[test]
fn missing_adapter_is_rejected_without_tempdir() {
    let env = Env::with_fake_codex();
    assert_eq!(
        assert_rejected(&env.run(&["codex", "--sandbox", "off", "--prompt", "hi"])),
        ADAPTER_MISSING
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
    let env = Env::with_fake_codex();
    let empty = env.root.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let output = env
        .command(&[
            "codex",
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
    assert_eq!(assert_rejected(&output), ADAPTER_MISSING);
    let output = env
        .command(&["codex", "--sandbox", "off", "--prompt", "hi"])
        .env("PATH", &empty)
        .output()
        .unwrap();
    assert_eq!(assert_rejected(&output), "codex not found in PATH");
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
