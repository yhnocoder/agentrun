#[allow(dead_code)]
mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use agentrun::cli::{Cli, Parsed};
use agentrun::doctor;
use agentrun::run::Caller;
use agentrun::signal::Signals;
use clap::Parser;
use serde_json::Value;
use support::WebServer;
use support::env::{Env, WAIT_LIMIT, poll_until, wait_until};
use support::process::{describe, process_is_gone};

const CLAUDE_LOGGED_IN: &str = "#!/bin/sh\nif [ \"$1\" = auth ] && [ \"$2\" = status ]; then printf '%s\\n' '{\"loggedIn\": true, \"authMethod\": \"claude.ai\"}'; exit 0; fi\nexit 0\n";
const PI_DEEPSEEK: &str = "#!/bin/sh\ncase \"$1\" in --version) echo 1.0.1; exit 0;; esac\necho '{\"type\":\"message_start\",\"message\":{\"role\":\"system\"}}'\necho '{\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"provider\":\"deepseek\",\"model\":\"deepseek-flash\"}}'\n/bin/sleep 30\n";
const PI_UNKNOWN_MODEL: &str = "#!/bin/sh\necho 'Warning: Model \"no-such-model\" not found for provider \"deepseek\". Using custom model id.' >&2\necho '{\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"provider\":\"deepseek\",\"model\":\"no-such-model\"}}'\n/bin/sleep 30\n";
const CODEX_LOGGED_IN: &str = "#!/bin/sh\nif [ \"$1\" = login ]; then echo 'Logged in using ChatGPT'; exit 0; fi\nif [ \"$1\" = sandbox ]; then while [ \"$1\" != -- ]; do shift; done; shift; exec \"$@\"; fi\nexit 0\n";

fn doctor(env: &Env, args: &[&str]) -> Command {
    let mut command = env.command(&[&["doctor"], args].concat());
    command.env("PATH", env.bin());
    command
}

fn run(env: &Env, args: &[&str]) -> Output {
    doctor(env, args).output().unwrap()
}

fn write_home(env: &Env, relative: &str, content: &str) -> PathBuf {
    let path = env.home().join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

fn pi_settings(env: &Env) {
    write_home(
        env,
        ".pi/agent/settings.json",
        r#"{"defaultProvider":"deepseek","defaultModel":"deepseek-flash"}"#,
    );
}

fn stdout_lines(output: &Output) -> Vec<String> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn line_for<'a>(lines: &'a [String], runtime: &str, check: &str) -> &'a str {
    lines
        .iter()
        .find(|line| {
            let mut fields = line.split_whitespace();
            fields.next();
            fields.next() == Some(runtime) && fields.next() == Some(check)
        })
        .unwrap_or_else(|| panic!("no {runtime} {check} line in {lines:#?}"))
}

fn assert_usage_error(output: &Output, expected: &str) {
    assert_eq!(output.status.code(), Some(2), "{}", stderr(output));
    assert!(output.stdout.is_empty());
    assert_eq!(stderr(output), format!("agentrun: {expected}\n"));
}

#[test]
fn runtimes_are_deduplicated_and_ordered() {
    let env = Env::new();
    env.install("claude", CLAUDE_LOGGED_IN);
    env.install("pi", PI_DEEPSEEK);
    pi_settings(&env);
    let output = run(&env, &["pi", "pi", "claude-code", "--sandbox", "off"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let lines = stdout_lines(&output);
    let runtimes: Vec<&str> = lines
        .iter()
        .map(|line| line.split_whitespace().nth(1).unwrap())
        .collect();
    assert_eq!(
        runtimes,
        [
            "claude-code",
            "claude-code",
            "claude-code",
            "claude-code",
            "pi",
            "pi",
            "pi",
            "pi"
        ]
    );
    let checks: Vec<&str> = lines[..4]
        .iter()
        .map(|line| line.split_whitespace().nth(2).unwrap())
        .collect();
    assert_eq!(checks, ["executable", "login", "sandbox", "network"]);
    assert!(stderr(&output).is_empty());
    assert!(env.leftovers().is_empty());
}

#[test]
fn usage_errors_exit_with_2_and_write_only_to_stderr() {
    let env = Env::new();
    let output = run(&env, &["--bogus"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).starts_with("agentrun: unexpected argument '--bogus'"));
    assert_usage_error(
        &run(&env, &["--network", "custom"]),
        "--network custom requires at least one --allow-host",
    );
    assert_usage_error(
        &run(&env, &["--allow-host", "example.com"]),
        "--allow-host requires --network custom",
    );
    assert_usage_error(
        &run(&env, &["--model", "flash"]),
        "--model for pi must be provider/model, got 'flash'",
    );
    assert_usage_error(
        &run(&env, &["claude-code", "--path", "/nonexistent-dir"]),
        "--path /nonexistent-dir: not a directory",
    );
    assert_usage_error(
        &run(&env, &["--env", "1bad=x"]),
        "--env: invalid variable name '1bad'",
    );
    let output = run(&env, &["--env-file", "/nonexistent-file"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).starts_with("agentrun: --env-file /nonexistent-file: "));
    let output = doctor(&env, &["pi"])
        .env("AGENTRUN_SANDBOX", "maybe")
        .output()
        .unwrap();
    assert_usage_error(
        &output,
        "invalid AGENTRUN_SANDBOX value 'maybe': expected on, relax or off",
    );
    assert!(env.leftovers().is_empty());
}

#[test]
fn credential_write_failure_exits_with_2() {
    let env = Env::new();
    env.install("pi", PI_DEEPSEEK);
    let output = doctor(&env, &["pi", "--sandbox", "off"])
        .env_remove("HOME")
        .env("AGENTRUN_PI_AUTH", "{}")
        .output()
        .unwrap();
    assert_usage_error(
        &output,
        "cannot write AGENTRUN_PI_AUTH to $HOME/.pi/agent/auth.json: HOME is not set",
    );
    assert!(env.leftovers().is_empty());
}

#[test]
fn missing_executable_fails_and_skips_login() {
    let env = Env::new();
    let output = run(&env, &["claude-code", "--sandbox", "off"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout_lines(&output),
        [
            "[fail] claude-code  executable  claude not found in PATH",
            "[skip] claude-code  login       executable not found",
            "[skip] claude-code  sandbox     --sandbox off",
            "[skip] claude-code  network     --network none",
        ]
    );
    let output = run(&env, &["codex", "--sandbox", "off", "--network", "full"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout_lines(&output),
        [
            "[fail] codex        executable  codex not found in PATH",
            "[skip] codex        login       executable not found",
            "[skip] codex        sandbox     --sandbox off",
            "[skip] codex        network     sandbox not running",
        ]
    );
    assert!(env.leftovers().is_empty());
}

#[test]
fn claude_code_login_reports_token_auth_method_and_failures() {
    let env = Env::new();
    env.install("claude", CLAUDE_LOGGED_IN);
    let output = run(&env, &["claude-code", "--sandbox", "off"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let lines = stdout_lines(&output);
    assert_eq!(
        line_for(&lines, "claude-code", "executable"),
        format!(
            "[ok]   claude-code  executable  {}",
            env.bin().join("claude").display()
        )
    );
    assert_eq!(
        line_for(&lines, "claude-code", "login"),
        "[ok]   claude-code  login       claude.ai"
    );
    let output = run(
        &env,
        &[
            "claude-code",
            "--sandbox",
            "off",
            "--env",
            "CLAUDE_CODE_OAUTH_TOKEN=secret-token",
        ],
    );
    let lines = stdout_lines(&output);
    assert_eq!(
        line_for(&lines, "claude-code", "login"),
        "[ok]   claude-code  login       CLAUDE_CODE_OAUTH_TOKEN"
    );
    assert!(!String::from_utf8(output.stdout).unwrap().contains("secret"));

    env.install(
        "claude",
        "#!/bin/sh\nprintf '%s\\n' '{\"loggedIn\": false}'\nexit 0\n",
    );
    let output = run(&env, &["claude-code", "--sandbox", "off"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        line_for(&stdout_lines(&output), "claude-code", "login"),
        "[fail] claude-code  login       not logged in. Run claude login, or set CLAUDE_CODE_OAUTH_TOKEN (from claude setup-token)"
    );

    env.install(
        "claude",
        "#!/bin/sh\necho 'something broke' >&2\necho 'second line' >&2\nexit 1\n",
    );
    let output = run(&env, &["claude-code", "--sandbox", "off"]);
    assert_eq!(
        line_for(&stdout_lines(&output), "claude-code", "login"),
        "[fail] claude-code  login       claude auth status failed: something broke"
    );

    env.install("claude", "#!/bin/sh\necho not json\nexit 0\n");
    let output = run(&env, &["claude-code", "--sandbox", "off"]);
    assert_eq!(
        line_for(&stdout_lines(&output), "claude-code", "login"),
        "[fail] claude-code  login       claude auth status failed: not json"
    );
    assert!(env.leftovers().is_empty());
}

#[test]
fn codex_login_needs_the_login_file_and_reads_login_status() {
    let env = Env::new();
    env.install("codex", CODEX_LOGGED_IN);
    let output = run(&env, &["codex", "--sandbox", "off"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        line_for(&stdout_lines(&output), "codex", "login"),
        format!(
            "[fail] codex        login       codex login file {} not found. Run codex login, or pass its content in AGENTRUN_CODEX_AUTH",
            env.home().join(".codex/auth.json").display()
        )
    );
    write_home(&env, ".codex/auth.json", "{}");
    let output = run(&env, &["codex", "--sandbox", "off", "--debug"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        line_for(&stdout_lines(&output), "codex", "login"),
        "[ok]   codex        login       Logged in using ChatGPT"
    );
    let debug = stderr(&output);
    assert!(
        debug.contains(&format!(
            "[debug] codex login command: {} login status\n",
            env.bin().join("codex").display()
        )),
        "{debug}"
    );
    let kept = debug
        .lines()
        .find_map(|line| line.strip_prefix("[debug] kept "))
        .unwrap_or_else(|| panic!("{debug}"));
    assert!(kept.starts_with(env.tmp().join("agentrun-doctor-").to_str().unwrap()));
    let home = PathBuf::from(kept).join("codex-home");
    assert!(std::fs::symlink_metadata(home.join("auth.json")).is_err());
    assert_eq!(
        std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
        0o700
    );
    std::fs::remove_dir_all(kept).unwrap();

    env.install("codex", "#!/bin/sh\necho 'Not logged in' >&2\nexit 1\n");
    let output = run(&env, &["codex", "--sandbox", "off"]);
    assert_eq!(
        line_for(&stdout_lines(&output), "codex", "login"),
        "[fail] codex        login       codex login status failed: Not logged in"
    );
    assert!(env.leftovers().is_empty());
}

#[test]
fn pi_login_compares_the_chosen_model() {
    let env = Env::new();
    env.install("pi", PI_DEEPSEEK);
    let output = run(&env, &["pi", "--sandbox", "off"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        line_for(&stdout_lines(&output), "pi", "login"),
        format!(
            "[fail] pi           login       no --model given and {} has no defaultProvider and defaultModel",
            env.home().join(".pi/agent/settings.json").display()
        )
    );
    pi_settings(&env);
    let started = Instant::now();
    let output = run(&env, &["pi", "--sandbox", "off"]);
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        line_for(&stdout_lines(&output), "pi", "login"),
        "[ok]   pi           login       deepseek/deepseek-flash (from pi settings)"
    );
    let output = run(
        &env,
        &[
            "pi",
            "--sandbox",
            "off",
            "--model",
            "deepseek/deepseek-flash:high",
        ],
    );
    assert_eq!(
        line_for(&stdout_lines(&output), "pi", "login"),
        "[ok]   pi           login       deepseek/deepseek-flash (from --model)"
    );
    let output = run(
        &env,
        &[
            "pi",
            "--sandbox",
            "off",
            "--model",
            "anthropic/claude-sonnet-4-5",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        line_for(&stdout_lines(&output), "pi", "login"),
        "[fail] pi           login       pi uses deepseek/deepseek-flash, expected anthropic/claude-sonnet-4-5 (from --model). Check the API key for anthropic"
    );
    env.install("pi", PI_UNKNOWN_MODEL);
    let output = run(
        &env,
        &[
            "pi",
            "--sandbox",
            "off",
            "--model",
            "deepseek/no-such-model",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        line_for(&stdout_lines(&output), "pi", "login"),
        "[fail] pi           login       pi does not know deepseek/no-such-model (from --model): Warning: Model \"no-such-model\" not found for provider \"deepseek\". Using custom model id."
    );
    env.install(
        "pi",
        "#!/bin/sh\necho '{\"type\":\"session\"}'\necho 'No API key found for provider deepseek' >&2\nexit 1\n",
    );
    let output = run(&env, &["pi", "--sandbox", "off"]);
    assert_eq!(
        line_for(&stdout_lines(&output), "pi", "login"),
        "[fail] pi           login       No API key found for provider deepseek"
    );
    env.install("pi", "#!/bin/sh\nexit 3\n");
    let output = run(&env, &["pi", "--sandbox", "off"]);
    assert_eq!(
        line_for(&stdout_lines(&output), "pi", "login"),
        "[fail] pi           login       pi exited with code 3 before choosing a model"
    );
    assert!(env.leftovers().is_empty());
}

#[test]
fn pi_login_times_out_when_pi_hangs() {
    let env = Env::new();
    env.install("pi", "#!/bin/sh\n/bin/sleep 30\n");
    pi_settings(&env);
    let cli = Cli::try_parse_from(["agentrun", "doctor", "pi", "--sandbox", "off"]).unwrap();
    let Parsed::Doctor(args) = cli.command.into_parsed() else {
        unreachable!("doctor arguments");
    };
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let caller = Caller {
        args: Vec::new(),
        env: vec![
            ("PATH".into(), env.bin().into_os_string()),
            ("TMPDIR".into(), env.tmp().into_os_string()),
            ("HOME".into(), env.home().into_os_string()),
        ],
        stdin: Box::new(std::io::empty()),
        stdin_is_terminal: false,
        stdout: Arc::clone(&stdout) as Arc<Mutex<dyn Write + Send>>,
        stdout_is_terminal: false,
        stderr: Arc::new(Mutex::new(Vec::new())),
        stderr_is_terminal: false,
        signals: Signals::install(),
    };
    let started = Instant::now();
    let code = doctor::run(caller, args, Duration::from_secs(1));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(code, 1);
    let text = String::from_utf8(stdout.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("[fail] pi           login       pi did not finish within 1 seconds\n"),
        "{text}"
    );
    assert!(env.leftovers().is_empty());
}

#[test]
fn sandbox_off_and_network_none_are_skipped() {
    let env = Env::new();
    env.install("pi", PI_DEEPSEEK);
    pi_settings(&env);
    let output = run(&env, &["pi", "--sandbox", "off", "--network", "full"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let lines = stdout_lines(&output);
    assert_eq!(
        line_for(&lines, "pi", "sandbox"),
        "[skip] pi           sandbox     --sandbox off"
    );
    assert_eq!(
        line_for(&lines, "pi", "network"),
        "[skip] pi           network     sandbox not running"
    );
    let output = doctor(&env, &["pi"])
        .env("AGENTRUN_SANDBOX", "off")
        .output()
        .unwrap();
    let lines = stdout_lines(&output);
    assert_eq!(
        line_for(&lines, "pi", "sandbox"),
        "[skip] pi           sandbox     --sandbox off"
    );
    assert_eq!(
        line_for(&lines, "pi", "network"),
        "[skip] pi           network     --network none"
    );
    assert!(env.leftovers().is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn sandbox_without_bwrap_fails_on_and_skips_relax() {
    let env = Env::new();
    env.install("claude", CLAUDE_LOGGED_IN);
    let output = run(&env, &["claude-code", "--network", "full"]);
    assert_eq!(output.status.code(), Some(1));
    let lines = stdout_lines(&output);
    assert_eq!(
        line_for(&lines, "claude-code", "sandbox"),
        "[fail] claude-code  sandbox     bwrap not found in PATH. Install bubblewrap and socat (for example: apt-get install bubblewrap socat, or dnf install bubblewrap socat), or use --sandbox relax or --sandbox off"
    );
    assert_eq!(
        line_for(&lines, "claude-code", "network"),
        "[skip] claude-code  network     sandbox not running"
    );
    let output = run(
        &env,
        &["claude-code", "--sandbox", "relax", "--network", "full"],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let lines = stdout_lines(&output);
    assert_eq!(
        line_for(&lines, "claude-code", "sandbox"),
        "[skip] claude-code  sandbox     bwrap not found in PATH"
    );
    assert_eq!(
        line_for(&lines, "claude-code", "network"),
        "[skip] claude-code  network     sandbox not running"
    );
    assert!(env.leftovers().is_empty());
}

#[cfg(target_os = "linux")]
fn real_bwrap_doctor() -> Option<(Env, PathBuf)> {
    let Some(bwrap) = support::system_bwrap() else {
        eprintln!("skipped: bwrap is not installed");
        return None;
    };
    if !support::sandbox_available() {
        return None;
    }
    let env = Env::new();
    env.install("claude", CLAUDE_LOGGED_IN);
    env.install("pi", PI_DEEPSEEK);
    env.install("codex", CODEX_LOGGED_IN);
    pi_settings(&env);
    write_home(&env, ".codex/auth.json", "{}");
    write_home(&env, ".pi/agent/auth.json", "{}");
    Some((env, bwrap.parent().unwrap().to_path_buf()))
}

#[cfg(target_os = "linux")]
fn doctor_with_bwrap(env: &Env, system: &Path, args: &[&str]) -> Output {
    let path = format!("{}:{}", env.bin().display(), system.display());
    doctor(env, args).env("PATH", path).output().unwrap()
}

#[cfg(target_os = "linux")]
fn kept_dir(debug: &str) -> PathBuf {
    PathBuf::from(
        debug
            .lines()
            .find_map(|line| line.strip_prefix("[debug] kept "))
            .unwrap_or_else(|| panic!("no kept directory in {debug}")),
    )
}

#[cfg(target_os = "linux")]
#[test]
fn real_bwrap_claude_code_sandbox_and_network_checks_pass() {
    let Some((env, system)) = real_bwrap_doctor() else {
        return;
    };
    let server = WebServer::start();
    let allow = format!("127.0.0.1:{}", server.port);
    let output = doctor_with_bwrap(
        &env,
        &system,
        &[
            "claude-code",
            "--network",
            "custom",
            "--allow-host",
            &allow,
            "--debug",
        ],
    );
    let lines = stdout_lines(&output);
    let debug = stderr(&output);
    if line_for(&lines, "claude-code", "sandbox").contains("bwrap cannot start") {
        eprintln!("skipped: {}", line_for(&lines, "claude-code", "sandbox"));
        return;
    }
    assert_eq!(
        line_for(&lines, "claude-code", "sandbox"),
        "[ok]   claude-code  sandbox     bubblewrap: wrote inside, blocked outside",
        "{debug}"
    );
    assert_eq!(
        line_for(&lines, "claude-code", "network"),
        format!(
            "[ok]   claude-code  network     {allow} reachable, doctor-check.invalid:443 refused (not_allowed)"
        ),
        "{debug}"
    );
    assert_eq!(output.status.code(), Some(0), "{lines:#?}\n{debug}");
    std::fs::remove_dir_all(kept_dir(&debug)).unwrap();
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
}

#[cfg(target_os = "linux")]
#[test]
fn real_bwrap_pi_checks_bind_the_state_dir_and_reach_the_allowed_host() {
    let Some((env, system)) = real_bwrap_doctor() else {
        return;
    };
    let server = WebServer::start();
    let allow = format!("127.0.0.1:{}", server.port);
    let output = doctor_with_bwrap(
        &env,
        &system,
        &[
            "pi",
            "--network",
            "custom",
            "--allow-host",
            &allow,
            "--debug",
        ],
    );
    let lines = stdout_lines(&output);
    let debug = stderr(&output);
    assert_eq!(
        line_for(&lines, "pi", "sandbox"),
        "[ok]   pi           sandbox     bubblewrap: wrote inside, blocked outside, pi started",
        "{debug}"
    );
    assert_eq!(
        line_for(&lines, "pi", "network"),
        format!(
            "[ok]   pi           network     {allow} reachable, doctor-check.invalid:443 refused (not_allowed)"
        ),
        "{debug}"
    );
    let pi_sandbox_commands: Vec<&str> = debug
        .lines()
        .filter(|line| line.starts_with("[debug] pi sandbox command: "))
        .collect();
    assert_eq!(pi_sandbox_commands.len(), 2, "{debug}");
    let state_dir = std::fs::canonicalize(env.home().join(".pi/agent")).unwrap();
    assert!(
        pi_sandbox_commands[0].contains(&format!(
            "--bind {0} {0} --ro-bind {0}/settings.json {0}/settings.json --dev /dev",
            state_dir.display()
        )),
        "{debug}"
    );
    assert!(
        pi_sandbox_commands[1].ends_with(&format!("{}/pi --version", env.bin().display())),
        "{debug}"
    );
    let network_command = debug
        .lines()
        .find(|line| line.starts_with("[debug] pi network command: "))
        .unwrap();
    assert!(
        network_command.contains(&format!(" {}/socat ", system.display()))
            && network_command.contains(" doctor-connect 127.0.0.1 "),
        "{network_command}"
    );
    let kept = kept_dir(&debug);
    assert!(kept.join("pi-sandbox/work/inside.txt").exists());
    assert!(!kept.join("pi-sandbox/outside.txt").exists());
    std::fs::remove_dir_all(kept).unwrap();
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
}

#[cfg(target_os = "linux")]
#[test]
fn real_bwrap_codex_sandbox_check_reports_the_outside_write() {
    let Some((env, system)) = real_bwrap_doctor() else {
        return;
    };
    let server = WebServer::start();
    let allow = format!("127.0.0.1:{}", server.port);
    let output = doctor_with_bwrap(
        &env,
        &system,
        &[
            "codex",
            "--network",
            "custom",
            "--allow-host",
            &allow,
            "--debug",
        ],
    );
    let lines = stdout_lines(&output);
    let debug = stderr(&output);
    assert_eq!(
        line_for(&lines, "codex", "sandbox"),
        "[fail] codex        sandbox     a write outside the allowed directories succeeded",
        "{debug}"
    );
    assert_eq!(output.status.code(), Some(1));
    let codex_command = debug
        .lines()
        .find(|line| line.starts_with("[debug] codex sandbox command: "))
        .unwrap();
    assert!(
        codex_command.contains(" sandbox -P agentrun -c 'permissions.agentrun.filesystem={")
            && codex_command.contains(" -c permissions.agentrun.network.enabled=false -C "),
        "{codex_command}"
    );
    let kept = kept_dir(&debug);
    assert!(kept.join("codex-sandbox/outside.txt").exists());
    std::fs::remove_dir_all(kept).unwrap();
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
}

#[cfg(target_os = "linux")]
#[test]
fn real_bwrap_network_check_fails_for_an_unreachable_host() {
    let Some((env, system)) = real_bwrap_doctor() else {
        return;
    };
    let output = doctor_with_bwrap(
        &env,
        &system,
        &[
            "claude-code",
            "--network",
            "custom",
            "--allow-host",
            "*.example",
        ],
    );
    let lines = stdout_lines(&output);
    assert!(
        line_for(&lines, "claude-code", "network")
            .starts_with("[fail] claude-code  network     www.example:443 not reachable: "),
        "{lines:#?}"
    );
    assert!(env.leftovers().is_empty(), "{:?}", env.leftovers());
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_runs_the_sandbox_and_network_checks() {
    let env = Env::new();
    env.install("claude", CLAUDE_LOGGED_IN);
    env.install("pi", PI_DEEPSEEK);
    pi_settings(&env);
    let server = WebServer::start();
    let allow = format!("127.0.0.1:{}", server.port);
    let output = run(
        &env,
        &[
            "claude-code",
            "pi",
            "--network",
            "custom",
            "--allow-host",
            &allow,
        ],
    );
    let lines = stdout_lines(&output);
    assert_eq!(output.status.code(), Some(0), "{lines:#?}");
    assert_eq!(
        line_for(&lines, "claude-code", "sandbox"),
        "[ok]   claude-code  sandbox     seatbelt: wrote inside, blocked outside"
    );
    assert_eq!(
        line_for(&lines, "pi", "sandbox"),
        "[ok]   pi           sandbox     seatbelt: wrote inside, blocked outside, pi started"
    );
    for runtime in ["claude-code", "pi"] {
        assert!(
            line_for(&lines, runtime, "network").ends_with(&format!(
                "network     {allow} reachable, doctor-check.invalid:443 refused (not_allowed)"
            )),
            "{lines:#?}"
        );
    }
    assert!(env.leftovers().is_empty());
}

struct ConnectProxy {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ConnectProxy {
    fn start(reply: &'static str) -> ConnectProxy {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                    head.push(byte[0]);
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head).into_owned());
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        ConnectProxy { port, requests }
    }
}

fn doctor_connect(env: &Env, proxy: Option<(&str, u16)>, host: &str, port: u16) -> Output {
    let mut command = env.command(&["doctor-connect", host, &port.to_string()]);
    command.env("PATH", env.bin());
    if let Some((name, proxy_port)) = proxy {
        command.env(name, format!("http://127.0.0.1:{proxy_port}"));
    }
    command.output().unwrap()
}

#[test]
fn doctor_connect_tunnels_through_the_proxy_or_connects_directly() {
    let env = Env::new();
    let accepting = ConnectProxy::start("HTTP/1.1 200 Connection Established\r\n\r\n");
    let output = doctor_connect(
        &env,
        Some(("https_proxy", accepting.port)),
        "example.com",
        443,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout_lines(&output), ["connected via proxy (200)"]);
    assert_eq!(
        accepting.requests.lock().unwrap().as_slice(),
        ["CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n"]
    );
    let refusing = ConnectProxy::start("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
    let output = doctor_connect(
        &env,
        Some(("HTTPS_PROXY", refusing.port)),
        "doctor-check.invalid",
        443,
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout_lines(&output),
        ["proxy replied HTTP/1.1 403 Forbidden"]
    );

    let server = WebServer::start();
    let output = doctor_connect(&env, None, "127.0.0.1", server.port);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout_lines(&output), ["connected"]);
    let closed_port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let output = doctor_connect(&env, None, "127.0.0.1", closed_port);
    assert_eq!(output.status.code(), Some(1));
    let lines = stdout_lines(&output);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("failed: "), "{lines:?}");

    let output = Command::new(env!("CARGO_BIN_EXE_agentrun"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("  doctor "), "{help}");
    assert!(!help.contains("doctor-connect"), "{help}");
}

#[test]
fn json_output_is_one_array_after_all_checks() {
    let env = Env::new();
    env.install("claude", CLAUDE_LOGGED_IN);
    let output = run(
        &env,
        &["claude-code", "codex", "--sandbox", "off", "--json"],
    );
    assert_eq!(output.status.code(), Some(1));
    let lines = stdout_lines(&output);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let items: Value = serde_json::from_str(&lines[0]).unwrap();
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 8);
    assert_eq!(
        items[0],
        serde_json::json!({
            "runtime": "claude-code",
            "check": "executable",
            "status": "ok",
            "detail": env.bin().join("claude").to_str().unwrap(),
        })
    );
    assert_eq!(
        items[4],
        serde_json::json!({
            "runtime": "codex",
            "check": "executable",
            "status": "fail",
            "detail": "codex not found in PATH",
        })
    );
    assert_eq!(items[6]["status"], "skip");
    assert_eq!(items[6]["detail"], "--sandbox off");
    assert!(env.leftovers().is_empty());
}

fn interrupt_during_pi_login(env: &Env, json: bool) -> Output {
    let marker = env.root().join("started");
    env.install(
        "pi",
        &format!("#!/bin/sh\necho $$ > {}\n/bin/sleep 30\n", marker.display()),
    );
    let mut args = vec!["pi", "--sandbox", "off"];
    if json {
        args.push("--json");
    }
    let child = doctor(env, &args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until("the fake pi to write its pid", || {
        std::fs::read_to_string(&marker).is_ok_and(|text| !text.trim().is_empty())
    });
    let pi_pid: i32 = std::fs::read_to_string(&marker)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        poll_until(WAIT_LIMIT, || process_is_gone(pi_pid)),
        "the fake pi is still running: {}",
        describe(pi_pid)
    );
    std::fs::remove_file(&marker).unwrap();
    output
}

#[test]
fn sigint_kills_the_check_and_keeps_the_finished_lines() {
    let env = Env::new();
    pi_settings(&env);
    let output = interrupt_during_pi_login(&env, false);
    assert_eq!(output.status.code(), Some(130));
    assert_eq!(
        stdout_lines(&output),
        [format!(
            "[ok]   pi           executable  {}",
            env.bin().join("pi").display()
        )]
    );
    assert!(env.leftovers().is_empty());
    let output = interrupt_during_pi_login(&env, true);
    assert_eq!(output.status.code(), Some(130));
    let lines = stdout_lines(&output);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let items: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(items.as_array().unwrap().len(), 1);
    assert_eq!(items[0]["check"], "executable");
    assert!(env.leftovers().is_empty());
}
