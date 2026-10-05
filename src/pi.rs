use std::ffi::OsString;
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::adapter::{Adapter, Launch, Record};
use crate::cli::NetworkMode;
use crate::cli::Runtime;
use crate::json::{first_line, joined_text, optional_string, string};
use crate::network::proxy_environment;
use crate::run::Invocation;
use crate::sandbox::{ProxyForward, wrap_pi, wrapper_failure};
use crate::usage::TokenCounts;

pub const STATE_DIR_VARIABLE: &str = "PI_CODING_AGENT_DIR";
pub const STATE_HOME_SUBDIR: &str = ".pi/agent";
const SERVICE_HOSTS: [(&str, &str); 5] = [
    ("deepseek", "api.deepseek.com"),
    ("anthropic", "api.anthropic.com"),
    ("openai", "api.openai.com"),
    ("google", "generativelanguage.googleapis.com"),
    ("openrouter", "openrouter.ai"),
];
const DEFAULT_PROVIDER_KEY: &str = "defaultProvider";
const PRIVATE_STATE_DIR: &str = "pi-agent";
const LOGIN_FILE: &str = "auth.json";
const MODELS_FILE: &str = "models.json";
const SETTINGS_FILE: &str = "settings.json";
const BIN_DIR: &str = "bin";
const LINKED_TOOLS: [&str; 2] = ["fd", "rg"];
const SETTINGS_KEYS: [&str; 2] = ["defaultProvider", "defaultModel"];
const FIXED_ARGS: [&str; 12] = [
    "-p",
    "--mode",
    "json",
    "--no-session",
    "--no-extensions",
    "--no-skills",
    "--no-prompt-templates",
    "--no-themes",
    "--no-context-files",
    "--no-approve",
    "--offline",
    "--tools",
];
const TOOLS: &str = "read,bash,edit,write,grep,find,ls";
const FAILED_STOP_REASONS: [&str; 2] = ["error", "aborted"];
const NO_REPLY_DETAIL: &str = "pi produced no model reply";
const DETAIL_MAX_CHARS: usize = 500;

pub struct Pi {
    sandboxed: bool,
    model_option: Option<String>,
    expected: Option<Model>,
    model_checked: bool,
    mismatch: Option<String>,
    prompt_echoed: bool,
    last_stop: Option<Stop>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Model {
    pub provider: String,
    pub model: String,
}

struct Stop {
    reason: String,
    error_message: Option<String>,
}

impl Default for Pi {
    fn default() -> Self {
        Self::new()
    }
}

impl Pi {
    pub fn new() -> Pi {
        Pi {
            sandboxed: false,
            model_option: None,
            expected: None,
            model_checked: false,
            mismatch: None,
            prompt_echoed: false,
            last_stop: None,
        }
    }

    fn message_start(&mut self, message: &Value) -> Vec<Record> {
        if message["role"] != "assistant" || self.model_checked {
            return Vec::new();
        }
        self.model_checked = true;
        let Some(expected) = &self.expected else {
            return Vec::new();
        };
        let provider = string(&message["provider"]);
        let model = string(&message["model"]);
        if provider == expected.provider && model == expected.model {
            return Vec::new();
        }
        self.mismatch = Some(format!(
            "pi uses {provider}/{model}, which does not match --model {}",
            self.model_option.as_deref().unwrap_or_default()
        ));
        vec![Record::Terminate]
    }

    fn message_end(&mut self, message: &Value) -> Vec<Record> {
        match message["role"].as_str() {
            Some("user") => {
                if self.prompt_echoed {
                    return Vec::new();
                }
                self.prompt_echoed = true;
                vec![Record::PromptEcho {
                    text: joined_text(&message["content"]),
                }]
            }
            Some("assistant") => {
                let mut records = Vec::new();
                let text = joined_text(&message["content"]);
                if !text.is_empty() {
                    records.push(Record::Text { parent: None, text });
                }
                records.push(Record::Usage {
                    parent: None,
                    model: model_name(message),
                    counts: counts(&message["usage"]),
                });
                self.last_stop = Some(Stop {
                    reason: string(&message["stopReason"]),
                    error_message: optional_string(&message["errorMessage"]),
                });
                records
            }
            _ => Vec::new(),
        }
    }
}

impl Adapter for Pi {
    fn runtime(&self) -> Runtime {
        Runtime::Pi
    }

    fn launch(&mut self, executable: &Path, invocation: &Invocation) -> Result<Launch, String> {
        self.sandboxed = invocation.sandbox.runs();
        self.model_option = invocation.args.model.clone();
        self.expected = match &invocation.args.model {
            Some(model) => Some(parse_model(model)?),
            None => None,
        };
        let private_dir = invocation.tempdir.join(PRIVATE_STATE_DIR);
        let user_dir =
            invocation
                .session
                .runtime_dir(&invocation.cwd, STATE_DIR_VARIABLE, STATE_HOME_SUBDIR);
        if !invocation.args.dry_run {
            prepare_state_dir(&private_dir, user_dir.as_deref()).map_err(|error| {
                format!(
                    "cannot prepare pi state directory {}: {error}",
                    private_dir.display()
                )
            })?;
        }
        let mut argv = vec![executable.to_string_lossy().into_owned()];
        argv.extend(FIXED_ARGS.iter().map(|arg| arg.to_string()));
        argv.push(TOOLS.to_string());
        if let Some(model) = &invocation.args.model {
            argv.push("--model".to_string());
            argv.push(model.clone());
        }
        if let Some(effort) = &invocation.args.effort {
            argv.push("--thinking".to_string());
            argv.push(effort.clone());
        }
        argv.extend(invocation.args.runtime_args.iter().cloned());
        argv.push("--".to_string());
        argv.push(invocation.prompt.clone());
        let mut env = vec![(
            OsString::from(STATE_DIR_VARIABLE),
            private_dir.into_os_string(),
        )];
        let mut service_hosts = Vec::new();
        let mut forward = None;
        if let Some(proxy) = &invocation.proxy {
            let provider = match &self.expected {
                Some(model) => Some(model.provider.clone()),
                None => user_dir
                    .as_deref()
                    .and_then(|dir| default_provider(&dir.join(SETTINGS_FILE))),
            };
            match provider.as_deref().and_then(service_host) {
                Some(host) => service_hosts.push(host.to_string()),
                None if invocation.args.network == NetworkMode::None => {
                    return Err(format!(
                        "cannot tell which host pi's model service uses (provider: {}). Use --network custom --allow-host <host of the model service>",
                        provider.as_deref().unwrap_or("unknown")
                    ));
                }
                None => {}
            }
            let socat = invocation
                .sandbox
                .socat
                .as_deref()
                .ok_or_else(|| "socat not found in PATH".to_string())?;
            forward = Some((socat.to_path_buf(), proxy.port_text(), proxy.socket.clone()));
            env.extend(proxy_environment(&proxy.port_text()));
        }
        let bwrap = invocation
            .sandbox
            .bwrap
            .as_deref()
            .filter(|_| self.sandboxed);
        if let Some(bwrap) = bwrap {
            let login = user_dir
                .as_deref()
                .and_then(|dir| std::fs::canonicalize(dir.join(LOGIN_FILE)).ok());
            let forward = forward.as_ref().map(|(socat, port, socket)| ProxyForward {
                socat,
                port,
                socket,
            });
            argv = wrap_pi(
                bwrap,
                &invocation.cwd,
                &invocation.tempdir,
                login.as_deref(),
                forward.as_ref(),
                &argv,
            );
        }
        Ok(Launch {
            argv,
            stdin: Vec::new(),
            env,
            signal_wrapped_child: bwrap.is_some(),
            service_hosts,
        })
    }

    fn echoes_prompt(&self) -> bool {
        true
    }

    fn translate(&mut self, line: &Value) -> Vec<Record> {
        match line["type"].as_str() {
            Some("tool_execution_start") => {
                let name = string(&line["toolName"]);
                let summary = tool_summary(&name, &line["args"]);
                vec![Record::ToolStart {
                    id: string(&line["toolCallId"]),
                    parent: None,
                    name,
                    summary,
                }]
            }
            Some("tool_execution_end") => vec![Record::ToolEnd {
                id: string(&line["toolCallId"]),
                denied: false,
            }],
            Some("message_start") => self.message_start(&line["message"]),
            Some("message_end") => self.message_end(&line["message"]),
            _ => Vec::new(),
        }
    }

    fn after_exit(&mut self) -> Vec<Record> {
        Vec::new()
    }

    fn failure(&self, exit_code: Option<i32>, stderr_tail: &str) -> Option<String> {
        if let Some(detail) = &self.mismatch {
            return Some(detail.clone());
        }
        if self.sandboxed
            && let Some(detail) = wrapper_failure(exit_code, stderr_tail)
        {
            return Some(detail);
        }
        if exit_code != Some(0) {
            return Some(String::new());
        }
        match &self.last_stop {
            Some(stop) if FAILED_STOP_REASONS.contains(&stop.reason.as_str()) => {
                Some(match &stop.error_message {
                    Some(message) => message.chars().take(DETAIL_MAX_CHARS).collect(),
                    None => format!("stopReason: {}", stop.reason),
                })
            }
            Some(_) => None,
            None => Some(NO_REPLY_DETAIL.to_string()),
        }
    }
}

pub fn parse_model(value: &str) -> Result<Model, String> {
    let name = value.split(':').next().unwrap_or_default();
    match name.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => Ok(Model {
            provider: provider.to_string(),
            model: model.to_string(),
        }),
        _ => Err(format!(
            "--model for pi must be provider/model, got '{value}'"
        )),
    }
}

fn prepare_state_dir(private_dir: &Path, user_dir: Option<&Path>) -> std::io::Result<()> {
    DirBuilder::new().mode(0o700).create(private_dir)?;
    let mut settings = Map::new();
    if let Some(user_dir) = user_dir {
        for name in [LOGIN_FILE, MODELS_FILE] {
            link_existing(&user_dir.join(name), &private_dir.join(name))?;
        }
        let tools: Vec<(PathBuf, PathBuf)> = LINKED_TOOLS
            .iter()
            .filter_map(|tool| {
                let real = std::fs::canonicalize(user_dir.join(BIN_DIR).join(tool)).ok()?;
                Some((real, private_dir.join(BIN_DIR).join(tool)))
            })
            .collect();
        if !tools.is_empty() {
            DirBuilder::new()
                .mode(0o700)
                .create(private_dir.join(BIN_DIR))?;
            for (real, link) in tools {
                std::os::unix::fs::symlink(real, link)?;
            }
        }
        settings = default_model_settings(&user_dir.join(SETTINGS_FILE));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(private_dir.join(SETTINGS_FILE))?;
    file.write_all(&serde_json::to_vec(&Value::Object(settings))?)?;
    Ok(())
}

fn link_existing(source: &Path, link: &Path) -> std::io::Result<()> {
    match std::fs::canonicalize(source) {
        Ok(real) => std::os::unix::fs::symlink(real, link),
        Err(_) => Ok(()),
    }
}

pub fn service_host(provider: &str) -> Option<&'static str> {
    SERVICE_HOSTS
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|(_, host)| *host)
}

fn default_provider(path: &Path) -> Option<String> {
    default_model_settings(path)
        .get(DEFAULT_PROVIDER_KEY)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn default_model_settings(path: &Path) -> Map<String, Value> {
    let mut settings = Map::new();
    let parsed = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    if let Some(Value::Object(user)) = parsed {
        for key in SETTINGS_KEYS {
            if let Some(value) = user.get(key) {
                settings.insert(key.to_string(), value.clone());
            }
        }
    }
    settings
}

fn model_name(message: &Value) -> Option<String> {
    match (message["provider"].as_str(), message["model"].as_str()) {
        (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
        _ => None,
    }
}

fn counts(usage: &Value) -> TokenCounts {
    TokenCounts {
        input_tokens: usage["input"].as_u64(),
        output_tokens: usage["output"].as_u64(),
        cache_read_tokens: usage["cacheRead"].as_u64(),
        cache_write_tokens: usage["cacheWrite"].as_u64(),
    }
}

fn tool_summary(name: &str, args: &Value) -> String {
    let value = match name {
        "bash" => args["command"].as_str().map(first_line),
        "read" | "write" | "edit" => args["path"].as_str().map(str::to_string),
        "ls" => Some(args["path"].as_str().unwrap_or(".").to_string()),
        "grep" | "find" => args["pattern"].as_str().map(str::to_string),
        _ => None,
    };
    match value {
        Some(value) => format!("{name}: {value}"),
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    use clap::Parser;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::cli::{Cli, Format, SandboxMode};
    use crate::event::SandboxKind;
    use crate::network::ProxyEndpoint;
    use crate::sandbox::Sandbox;
    use crate::session::{Session, parse_env_args};

    struct Setup {
        root: TempDir,
    }

    impl Setup {
        fn new() -> Setup {
            let setup = Setup {
                root: tempfile::tempdir().unwrap(),
            };
            std::fs::create_dir(setup.work()).unwrap();
            std::fs::create_dir(setup.tempdir()).unwrap();
            setup
        }

        fn work(&self) -> PathBuf {
            self.root.path().join("work")
        }

        fn tempdir(&self) -> PathBuf {
            self.root.path().join("agentrun-session")
        }

        fn home(&self) -> PathBuf {
            self.root.path().join("home")
        }

        fn user_state(&self) -> PathBuf {
            self.home().join(".pi/agent")
        }

        fn private(&self) -> PathBuf {
            self.tempdir().join("pi-agent")
        }

        fn write_user_file(&self, name: &str, content: &str) -> PathBuf {
            let path = self.user_state().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            path
        }

        fn invocation(&self, env: &[(&str, &str)], sandboxed: bool, extra: &[&str]) -> Invocation {
            let mut args = vec!["agentrun", "pi", "--prompt", "hi"];
            args.extend(extra);
            let (runtime, args) = Cli::try_parse_from(args).unwrap().command.into_parts();
            let caller_env: Vec<(OsString, OsString)> = env
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                .collect();
            let env_args = parse_env_args(&args.env, &caller_env).unwrap();
            let session = Session::assemble(runtime, &caller_env, &[], &[], &env_args);
            Invocation {
                runtime,
                args,
                cwd: self.work(),
                prompt: "hi".to_string(),
                format: Format::Jsonl,
                sandbox: Sandbox {
                    mode: SandboxMode::On,
                    kind: if sandboxed {
                        SandboxKind::Bubblewrap
                    } else {
                        SandboxKind::None
                    },
                    reason: String::new(),
                    bwrap: sandboxed.then(|| PathBuf::from("/usr/bin/bwrap")),
                    socat: sandboxed.then(|| PathBuf::from("/usr/bin/socat")),
                    description: String::new(),
                },
                tempdir: self.tempdir(),
                session,
                allow_hosts: Vec::new(),
                proxy: None,
                codex_home: None,
            }
        }

        fn with_home(&self) -> Vec<(String, String)> {
            vec![(
                "HOME".to_string(),
                self.home().to_string_lossy().into_owned(),
            )]
        }
    }

    fn pairs(list: &[(String, String)]) -> Vec<(&str, &str)> {
        list.iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect()
    }

    fn launch(setup: &Setup, env: &[(&str, &str)], sandboxed: bool, extra: &[&str]) -> Launch {
        Pi::new()
            .launch(
                Path::new("/opt/bin/pi"),
                &setup.invocation(env, sandboxed, extra),
            )
            .unwrap()
    }

    fn fixed(executable: &str) -> Vec<String> {
        let mut argv = vec![executable.to_string()];
        argv.extend(FIXED_ARGS.iter().map(|arg| arg.to_string()));
        argv.push(TOOLS.to_string());
        argv
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn arguments_without_options() {
        let setup = Setup::new();
        let launch = launch(&setup, &[], false, &[]);
        let mut expected = fixed("/opt/bin/pi");
        expected.extend(["--", "hi"].map(str::to_string));
        assert_eq!(launch.argv, expected);
        assert!(launch.stdin.is_empty());
        assert!(!launch.signal_wrapped_child);
        assert_eq!(
            launch.env,
            vec![(
                OsString::from("PI_CODING_AGENT_DIR"),
                setup.private().into_os_string()
            )]
        );
    }

    #[test]
    fn model_effort_and_caller_arguments_come_before_the_prompt() {
        let setup = Setup::new();
        let launch = launch(
            &setup,
            &[],
            false,
            &[
                "--model",
                "deepseek/deepseek-flash:high",
                "--effort",
                "low",
                "--",
                "--verbose",
                "-x",
            ],
        );
        assert_eq!(
            launch.argv[fixed("").len()..],
            [
                "--model",
                "deepseek/deepseek-flash:high",
                "--thinking",
                "low",
                "--verbose",
                "-x",
                "--",
                "hi",
            ]
        );
    }

    #[test]
    fn prompt_starting_with_a_dash_stays_after_the_separator() {
        let setup = Setup::new();
        let mut invocation = setup.invocation(&[], false, &[]);
        invocation.prompt = "-n list files".to_string();
        let launch = Pi::new()
            .launch(Path::new("/opt/bin/pi"), &invocation)
            .unwrap();
        assert_eq!(
            launch.argv[launch.argv.len() - 2..],
            ["--", "-n list files"]
        );
    }

    #[test]
    fn sandboxed_argv_binds_the_real_login_file() {
        let setup = Setup::new();
        let real = setup.write_user_file("real-auth.json", "{}");
        std::os::unix::fs::symlink(&real, setup.user_state().join("auth.json")).unwrap();
        let home = setup.with_home();
        let launch = launch(&setup, &pairs(&home), true, &[]);
        assert!(launch.signal_wrapped_child);
        let real_text = std::fs::canonicalize(&real)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let position = launch
            .argv
            .iter()
            .position(|arg| *arg == real_text)
            .expect("the real login path is bound");
        assert_eq!(launch.argv[position - 1], "--bind");
        assert_eq!(launch.argv[position + 1], real_text);
        assert!(
            !launch
                .argv
                .iter()
                .any(|arg| arg.ends_with("/agent/auth.json"))
        );
        assert_eq!(launch.argv[0], "/usr/bin/bwrap");
        let separator = launch.argv.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(launch.argv[separator + 1], "/opt/bin/pi");
    }

    #[test]
    fn sandboxed_argv_without_login_file_binds_only_cwd_and_tempdir() {
        let setup = Setup::new();
        let home = setup.with_home();
        let launch = launch(&setup, &pairs(&home), true, &[]);
        assert_eq!(launch.argv.iter().filter(|arg| *arg == "--bind").count(), 2);
    }

    #[test]
    fn private_dir_links_existing_user_files() {
        let setup = Setup::new();
        let auth = setup.write_user_file("auth.json", "{\"deepseek\":{}}");
        let models = setup.write_user_file("models.json", "{}");
        let fd = setup.write_user_file("bin/fd", "#!/bin/sh\n");
        setup.write_user_file(
            "settings.json",
            "{\"defaultProvider\":\"deepseek\",\"defaultModel\":\"deepseek-flash\",\"packages\":[\"x\"]}",
        );
        let home = setup.with_home();
        launch(&setup, &pairs(&home), false, &[]);
        let private = setup.private();
        assert_eq!(mode(&private), 0o700);
        assert_eq!(
            std::fs::read_link(private.join("auth.json")).unwrap(),
            std::fs::canonicalize(&auth).unwrap()
        );
        assert_eq!(
            std::fs::read_link(private.join("models.json")).unwrap(),
            std::fs::canonicalize(&models).unwrap()
        );
        assert_eq!(mode(&private.join("bin")), 0o700);
        assert_eq!(
            std::fs::read_link(private.join("bin/fd")).unwrap(),
            std::fs::canonicalize(&fd).unwrap()
        );
        assert!(!private.join("bin/rg").exists());
        assert_eq!(mode(&private.join("settings.json")), 0o600);
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(private.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            settings,
            json!({"defaultProvider": "deepseek", "defaultModel": "deepseek-flash"})
        );
        let names: Vec<String> = std::fs::read_dir(&private)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn private_dir_links_resolve_symlinked_user_files() {
        let setup = Setup::new();
        let real = setup.write_user_file("elsewhere/rg", "#!/bin/sh\n");
        std::fs::create_dir_all(setup.user_state().join("bin")).unwrap();
        std::os::unix::fs::symlink(&real, setup.user_state().join("bin/rg")).unwrap();
        let home = setup.with_home();
        launch(&setup, &pairs(&home), false, &[]);
        let private = setup.private();
        assert_eq!(
            std::fs::read_link(private.join("bin/rg")).unwrap(),
            std::fs::canonicalize(&real).unwrap()
        );
        assert!(!private.join("bin/fd").exists());
        assert!(!private.join("auth.json").exists());
        assert!(!private.join("models.json").exists());
    }

    #[test]
    fn private_dir_without_user_files_has_only_empty_settings() {
        let setup = Setup::new();
        for env in [
            Vec::new(),
            setup.with_home(),
            vec![(
                "PI_CODING_AGENT_DIR".to_string(),
                setup
                    .root
                    .path()
                    .join("missing")
                    .to_string_lossy()
                    .into_owned(),
            )],
        ] {
            let _ = std::fs::remove_dir_all(setup.private());
            launch(&setup, &pairs(&env), false, &[]);
            let names: Vec<String> = std::fs::read_dir(setup.private())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, ["settings.json"], "{env:?}");
            assert_eq!(
                std::fs::read_to_string(setup.private().join("settings.json")).unwrap(),
                "{}"
            );
        }
    }

    #[test]
    fn settings_that_are_missing_or_not_json_objects_become_empty() {
        let setup = Setup::new();
        let home = setup.with_home();
        for content in ["not json", "[1,2]", "\"text\"", ""] {
            let _ = std::fs::remove_dir_all(setup.private());
            setup.write_user_file("settings.json", content);
            launch(&setup, &pairs(&home), false, &[]);
            assert_eq!(
                std::fs::read_to_string(setup.private().join("settings.json")).unwrap(),
                "{}",
                "{content}"
            );
        }
        let _ = std::fs::remove_dir_all(setup.private());
        setup.write_user_file(
            "settings.json",
            "{\"defaultModel\": 3, \"theme\": \"dark\"}",
        );
        launch(&setup, &pairs(&home), false, &[]);
        assert_eq!(
            std::fs::read_to_string(setup.private().join("settings.json")).unwrap(),
            "{\"defaultModel\":3}"
        );
    }

    #[test]
    fn state_dir_variable_from_env_option_selects_the_user_files() {
        let setup = Setup::new();
        let custom = setup.root.path().join("custom-state");
        std::fs::create_dir(&custom).unwrap();
        std::fs::write(custom.join("auth.json"), "{}").unwrap();
        let home = setup.with_home();
        let variable = format!("PI_CODING_AGENT_DIR={}", custom.display());
        launch(&setup, &pairs(&home), false, &["--env", &variable]);
        assert_eq!(
            std::fs::read_link(setup.private().join("auth.json")).unwrap(),
            std::fs::canonicalize(custom.join("auth.json")).unwrap()
        );
    }

    fn proxy(port: Option<u16>, setup: &Setup) -> ProxyEndpoint {
        ProxyEndpoint {
            port,
            socket: setup.tempdir().join("proxy.sock"),
        }
    }

    fn proxied(setup: &Setup, port: Option<u16>, extra: &[&str]) -> Result<Launch, String> {
        let _ = std::fs::remove_dir_all(setup.private());
        let mut invocation = setup.invocation(&pairs(&setup.with_home()), true, extra);
        invocation.proxy = Some(proxy(port, setup));
        Pi::new().launch(Path::new("/opt/bin/pi"), &invocation)
    }

    #[test]
    fn proxied_launch_forwards_through_socat_and_sets_proxy_variables() {
        let setup = Setup::new();
        let launch = proxied(&setup, Some(41234), &["--model", "deepseek/deepseek-flash"]).unwrap();
        assert_eq!(launch.service_hosts, ["api.deepseek.com"]);
        let separator = launch.argv.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(
            launch.argv[separator + 1..separator + 7],
            [
                "/bin/sh",
                "-c",
                r#""$0" "TCP-LISTEN:$1,bind=127.0.0.1,fork,reuseaddr" "UNIX-CONNECT:$2" 2>/dev/null & shift 2; exec "$@""#,
                "/usr/bin/socat",
                "41234",
                setup.tempdir().join("proxy.sock").to_str().unwrap(),
            ]
        );
        assert_eq!(launch.argv[separator + 7], "/opt/bin/pi");
        assert!(launch.argv.contains(&"--unshare-net".to_string()));
        let address = OsString::from("http://127.0.0.1:41234");
        let mut expected = vec![(
            OsString::from("PI_CODING_AGENT_DIR"),
            setup.private().into_os_string(),
        )];
        for name in [
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "ALL_PROXY",
            "https_proxy",
            "http_proxy",
            "all_proxy",
        ] {
            expected.push((OsString::from(name), address.clone()));
        }
        for name in ["NO_PROXY", "no_proxy"] {
            expected.push((OsString::from(name), OsString::new()));
        }
        assert_eq!(launch.env, expected);
    }

    #[test]
    fn proxied_launch_takes_the_provider_from_user_settings() {
        let setup = Setup::new();
        setup.write_user_file("settings.json", "{\"defaultProvider\":\"openai\"}");
        let launch = proxied(&setup, Some(1), &[]).unwrap();
        assert_eq!(launch.service_hosts, ["api.openai.com"]);
        for (provider, host) in [
            ("anthropic", "api.anthropic.com"),
            ("google", "generativelanguage.googleapis.com"),
            ("openrouter", "openrouter.ai"),
        ] {
            let launch = proxied(&setup, Some(1), &["--model", &format!("{provider}/m")]).unwrap();
            assert_eq!(launch.service_hosts, [host]);
        }
    }

    #[test]
    fn proxied_launch_without_a_known_provider_depends_on_the_network_mode() {
        let setup = Setup::new();
        let unknown = "cannot tell which host pi's model service uses (provider: unknown). Use --network custom --allow-host <host of the model service>";
        assert_eq!(proxied(&setup, Some(1), &[]).unwrap_err(), unknown);
        setup.write_user_file("settings.json", "not json");
        assert_eq!(proxied(&setup, Some(1), &[]).unwrap_err(), unknown);
        setup.write_user_file("settings.json", "{\"defaultProvider\":5}");
        assert_eq!(proxied(&setup, Some(1), &[]).unwrap_err(), unknown);
        assert_eq!(
            proxied(&setup, Some(1), &["--model", "acme/robot"]).unwrap_err(),
            "cannot tell which host pi's model service uses (provider: acme). Use --network custom --allow-host <host of the model service>"
        );
        let custom = proxied(
            &setup,
            Some(1),
            &[
                "--model",
                "acme/robot",
                "--network",
                "custom",
                "--allow-host",
                "robot.example",
            ],
        )
        .unwrap();
        assert!(custom.service_hosts.is_empty());
        let full = proxied(
            &setup,
            Some(1),
            &["--model", "acme/robot", "--network", "full"],
        )
        .unwrap();
        assert!(full.service_hosts.is_empty());
    }

    #[test]
    fn proxied_dry_run_writes_the_port_placeholder() {
        let setup = Setup::new();
        let launch = proxied(
            &setup,
            None,
            &["--dry-run", "--model", "deepseek/deepseek-flash"],
        )
        .unwrap();
        assert!(launch.argv.contains(&"<proxy port>".to_string()));
        assert!(launch.env.contains(&(
            OsString::from("HTTPS_PROXY"),
            OsString::from("http://127.0.0.1:<proxy port>")
        )));
        assert!(!setup.private().exists());
    }

    #[test]
    fn proxied_launch_without_socat_is_an_error() {
        let setup = Setup::new();
        let mut invocation = setup.invocation(&[], true, &["--model", "deepseek/deepseek-flash"]);
        invocation.proxy = Some(proxy(Some(1), &setup));
        invocation.sandbox.socat = None;
        assert_eq!(
            Pi::new()
                .launch(Path::new("/opt/bin/pi"), &invocation)
                .unwrap_err(),
            "socat not found in PATH"
        );
    }

    #[test]
    fn dry_run_creates_nothing() {
        let setup = Setup::new();
        let home = setup.with_home();
        setup.write_user_file("auth.json", "{}");
        let launch = launch(&setup, &pairs(&home), false, &["--dry-run"]);
        assert!(!setup.private().exists());
        assert_eq!(
            launch.env[0].1,
            setup.private().into_os_string(),
            "{launch:?}"
        );
    }

    #[test]
    fn unwritable_tempdir_is_a_launch_error() {
        let setup = Setup::new();
        let mut invocation = setup.invocation(&[], false, &[]);
        invocation.tempdir = setup.root.path().join("missing-tempdir");
        let detail = Pi::new()
            .launch(Path::new("/opt/bin/pi"), &invocation)
            .unwrap_err();
        assert!(
            detail.starts_with(&format!(
                "cannot prepare pi state directory {}: ",
                invocation.tempdir.join("pi-agent").display()
            )),
            "{detail}"
        );
    }

    #[test]
    fn model_format_cases() {
        let ok = |value: &str, provider: &str, model: &str| {
            assert_eq!(
                parse_model(value),
                Ok(Model {
                    provider: provider.to_string(),
                    model: model.to_string()
                }),
                "{value}"
            );
        };
        ok("deepseek/deepseek-flash", "deepseek", "deepseek-flash");
        ok("deepseek/deepseek-flash:high", "deepseek", "deepseek-flash");
        ok("a/b/c", "a", "b/c");
        for value in ["deepseek-flash", "/x", "x/", "x/:high", "", ":"] {
            assert_eq!(
                parse_model(value),
                Err(format!(
                    "--model for pi must be provider/model, got '{value}'"
                )),
                "{value}"
            );
        }
        let setup = Setup::new();
        let detail = Pi::new()
            .launch(
                Path::new("/opt/bin/pi"),
                &setup.invocation(&[], false, &["--model", "deepseek-flash"]),
            )
            .unwrap_err();
        assert_eq!(
            detail,
            "--model for pi must be provider/model, got 'deepseek-flash'"
        );
    }

    fn adapter_with_model(setup: &Setup, model: Option<&str>) -> Pi {
        let _ = std::fs::remove_dir_all(setup.private());
        let mut adapter = Pi::new();
        let extra: Vec<&str> = match model {
            Some(model) => vec!["--model", model],
            None => Vec::new(),
        };
        adapter
            .launch(
                Path::new("/opt/bin/pi"),
                &setup.invocation(&[], false, &extra),
            )
            .unwrap();
        adapter
    }

    fn start(provider: &str, model: &str) -> Value {
        json!({"type": "message_start", "message": {"role": "assistant", "content": [], "provider": provider, "model": model, "stopReason": "pending"}})
    }

    #[test]
    fn model_check_cases() {
        let setup = Setup::new();
        let cases = [
            (
                "deepseek/deepseek-flash",
                "deepseek",
                "deepseek-flash",
                true,
            ),
            ("deepseek/deepseek-flash", "openai", "deepseek-flash", false),
            (
                "deepseek/deepseek-flash",
                "deepseek",
                "deepseek-v4-pro",
                false,
            ),
            (
                "deepseek/deepseek-flash",
                "DeepSeek",
                "deepseek-flash",
                false,
            ),
            (
                "deepseek/deepseek-flash:high",
                "deepseek",
                "deepseek-flash",
                true,
            ),
            ("a/b/c", "a", "b/c", true),
        ];
        for (option, provider, model, matches) in cases {
            let mut adapter = adapter_with_model(&setup, Some(option));
            let records = adapter.translate(&start(provider, model));
            if matches {
                assert_eq!(records, vec![], "{option} {provider}/{model}");
                assert_eq!(
                    adapter.failure(Some(0), ""),
                    Some(NO_REPLY_DETAIL.to_string())
                );
            } else {
                assert_eq!(
                    records,
                    vec![Record::Terminate],
                    "{option} {provider}/{model}"
                );
                assert_eq!(
                    adapter.failure(None, ""),
                    Some(format!(
                        "pi uses {provider}/{model}, which does not match --model {option}"
                    ))
                );
            }
            assert_eq!(adapter.translate(&start("other", "model")), vec![]);
        }
        let mut unchecked = adapter_with_model(&setup, None);
        assert_eq!(unchecked.translate(&start("other", "model")), vec![]);
        assert_eq!(unchecked.failure(None, ""), Some(String::new()));
        let mut system_first = adapter_with_model(&setup, Some("deepseek/deepseek-flash"));
        let system = json!({"type": "message_start", "message": {"role": "system", "content": ""}});
        assert_eq!(system_first.translate(&system), vec![]);
        assert_eq!(
            system_first.translate(&start("openai", "gpt")),
            vec![Record::Terminate]
        );
    }

    #[test]
    fn summary_per_tool_and_missing_fields() {
        let cases = [
            (
                "bash",
                json!({"command": "ls -la\necho done", "timeout": 30}),
                "bash: ls -la",
            ),
            ("read", json!({"path": "src/main.rs"}), "read: src/main.rs"),
            (
                "write",
                json!({"path": "a.txt", "content": "x"}),
                "write: a.txt",
            ),
            ("edit", json!({"path": "/etc/hosts"}), "edit: /etc/hosts"),
            ("ls", json!({"path": "src"}), "ls: src"),
            ("ls", json!({}), "ls: ."),
            ("ls", json!({"path": null}), "ls: ."),
            ("grep", json!({"pattern": "fn main"}), "grep: fn main"),
            ("find", json!({"pattern": "*.toml"}), "find: *.toml"),
            ("bash", json!({}), "bash"),
            ("read", json!({"path": 3}), "read"),
            ("grep", json!({"path": "."}), "grep"),
            ("fetch", json!({"url": "https://example.com"}), "fetch"),
        ];
        for (name, args, expected) in cases {
            assert_eq!(tool_summary(name, &args), expected, "{name} {args}");
        }
    }

    fn assistant_end(blocks: Value, stop: &str, error: Option<&str>) -> Value {
        let mut message = json!({
            "role": "assistant",
            "content": blocks,
            "api": "openai-completions",
            "provider": "deepseek",
            "model": "deepseek-flash",
            "usage": {"input": 10, "output": 2, "cacheRead": 5, "cacheWrite": 0, "reasoning": 1, "totalTokens": 17, "cost": {"total": 0.1}},
            "stopReason": stop,
        });
        if let Some(error) = error {
            message["errorMessage"] = json!(error);
        }
        json!({"type": "message_end", "message": message})
    }

    #[test]
    fn tool_and_message_events_become_records() {
        let mut adapter = Pi::new();
        let start = json!({"type": "tool_execution_start", "toolCallId": "call_1", "toolName": "write", "args": {"path": "probe.txt", "content": "hello"}});
        assert_eq!(
            adapter.translate(&start),
            vec![Record::ToolStart {
                id: "call_1".to_string(),
                parent: None,
                name: "write".to_string(),
                summary: "write: probe.txt".to_string(),
            }]
        );
        let end = json!({"type": "tool_execution_end", "toolCallId": "call_1", "toolName": "write", "result": {"content": []}, "isError": true});
        assert_eq!(
            adapter.translate(&end),
            vec![Record::ToolEnd {
                id: "call_1".to_string(),
                denied: false,
            }]
        );
        let user = json!({"type": "message_end", "message": {"role": "user", "content": [{"type": "text", "text": "do"}, {"type": "text", "text": "it"}]}});
        assert_eq!(
            adapter.translate(&user),
            vec![Record::PromptEcho {
                text: "do\nit".to_string()
            }]
        );
        assert_eq!(adapter.translate(&user), vec![]);
        let plain_user =
            json!({"type": "message_end", "message": {"role": "user", "content": "plain"}});
        assert_eq!(adapter.translate(&plain_user), vec![]);
        let blocks = json!([
            {"type": "thinking", "thinking": "hmm"},
            {"type": "text", "text": "first"},
            {"type": "toolCall", "id": "c", "name": "read", "arguments": {}},
            {"type": "text", "text": "second"}
        ]);
        assert_eq!(
            adapter.translate(&assistant_end(blocks, "toolUse", None)),
            vec![
                Record::Text {
                    parent: None,
                    text: "first\nsecond".to_string()
                },
                Record::Usage {
                    parent: None,
                    model: Some("deepseek/deepseek-flash".to_string()),
                    counts: TokenCounts {
                        input_tokens: Some(10),
                        output_tokens: Some(2),
                        cache_read_tokens: Some(5),
                        cache_write_tokens: Some(0),
                    },
                }
            ]
        );
        let no_text = json!([{"type": "toolCall", "id": "c", "name": "read", "arguments": {}}]);
        let records = adapter.translate(&assistant_end(no_text, "toolUse", None));
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0], Record::Usage { .. }));
        let no_usage = json!({"type": "message_end", "message": {"role": "assistant", "content": [], "stopReason": "stop"}});
        assert_eq!(
            adapter.translate(&no_usage),
            vec![Record::Usage {
                parent: None,
                model: None,
                counts: TokenCounts::default(),
            }]
        );
        for ignored in [
            json!({"type": "session", "version": 3}),
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({"type": "message_update", "message": {"role": "assistant"}}),
            json!({"type": "message_end", "message": {"role": "toolResult", "content": [{"type": "text", "text": "x"}]}}),
            json!({"type": "message_end", "message": {"role": "system", "content": "x"}}),
            json!({"type": "turn_end"}),
            json!({"type": "agent_end"}),
            json!({"type": "agent_settled"}),
            json!({"no": "type"}),
        ] {
            assert_eq!(adapter.translate(&ignored), vec![], "{ignored}");
        }
        assert_eq!(adapter.after_exit(), vec![]);
        let plain_prompt = Pi::new().translate(&plain_user);
        assert_eq!(
            plain_prompt,
            vec![Record::PromptEcho {
                text: "plain".to_string()
            }]
        );
    }

    #[test]
    fn failure_cases_in_order() {
        let setup = Setup::new();
        let bwrap_line = "bwrap: Can't mkdir /x: Permission denied";
        let mut sandboxed = Pi::new();
        sandboxed
            .launch(
                Path::new("/opt/bin/pi"),
                &setup.invocation(&[], true, &["--model", "deepseek/deepseek-flash"]),
            )
            .unwrap();
        assert_eq!(
            sandboxed.failure(Some(1), bwrap_line),
            Some("sandbox failed to start: Can't mkdir /x: Permission denied".to_string())
        );
        assert_eq!(
            sandboxed.failure(Some(0), bwrap_line),
            Some(NO_REPLY_DETAIL.to_string())
        );
        assert_eq!(
            sandboxed.translate(&start("openai", "gpt")),
            vec![Record::Terminate]
        );
        assert_eq!(
            sandboxed.failure(Some(1), bwrap_line),
            Some(
                "pi uses openai/gpt, which does not match --model deepseek/deepseek-flash"
                    .to_string()
            )
        );

        let mut open = Pi::new();
        assert_eq!(open.failure(Some(1), bwrap_line), Some(String::new()));
        assert_eq!(open.failure(None, ""), Some(String::new()));
        assert_eq!(open.failure(Some(0), ""), Some(NO_REPLY_DETAIL.to_string()));
        open.translate(&assistant_end(json!([]), "error", Some("401: bad key")));
        assert_eq!(open.failure(Some(0), ""), Some("401: bad key".to_string()));
        assert_eq!(open.failure(Some(1), ""), Some(String::new()));
        open.translate(&assistant_end(json!([]), "aborted", None));
        assert_eq!(
            open.failure(Some(0), ""),
            Some("stopReason: aborted".to_string())
        );
        let long = "é".repeat(700);
        open.translate(&assistant_end(json!([]), "error", Some(&long)));
        assert_eq!(open.failure(Some(0), ""), Some("é".repeat(500)));
        open.translate(&assistant_end(json!([]), "length", None));
        assert_eq!(open.failure(Some(0), ""), None);
        open.translate(&assistant_end(
            json!([{"type": "text", "text": "ok"}]),
            "stop",
            None,
        ));
        assert_eq!(open.failure(Some(0), ""), None);
        open.translate(&assistant_end(json!([]), "error", None));
        assert_eq!(
            open.failure(Some(0), ""),
            Some("stopReason: error".to_string())
        );
    }
}
