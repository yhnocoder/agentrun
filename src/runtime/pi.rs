use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Adapter, Invocation, Launch, detail_head};
use crate::cli::NetworkMode;
use crate::cli::Runtime;
use crate::json::{first_line, joined_text, optional_string, string};
use crate::network::proxy_environment;
use crate::output::{Record, SandboxKind, TokenCounts};
use crate::sandbox::{self, PiState, wrapper_failure};
use crate::session::Session;

const STATE_DIR_VARIABLE: &str = "PI_CODING_AGENT_DIR";
pub(crate) const STATE_HOME_SUBDIR: &str = ".pi/agent";
const SERVICE_HOSTS: [(&str, &str); 5] = [
    ("deepseek", "api.deepseek.com"),
    ("anthropic", "api.anthropic.com"),
    ("openai", "api.openai.com"),
    ("google", "generativelanguage.googleapis.com"),
    ("openrouter", "openrouter.ai"),
];
const DEFAULT_PROVIDER_KEY: &str = "defaultProvider";
const DEFAULT_MODEL_KEY: &str = "defaultModel";
pub(crate) const LOGIN_FILE: &str = "auth.json";
pub(crate) const SETTINGS_FILE: &str = "settings.json";
const WRITABLE_STATE_ENTRIES: [&str; 5] = [
    "auth.json",
    "auth.json.lock",
    "models-store.json",
    "models-store.json.lock",
    "settings.json.lock",
];
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

pub struct Pi {
    sandbox: SandboxKind,
    model_option: Option<String>,
    expected: Option<Model>,
    model_checked: bool,
    mismatch: Option<String>,
    prompt_echoed: bool,
    last_stop: Option<Stop>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Model {
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
            sandbox: SandboxKind::None,
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
        if assistant_model(message).as_ref() == Some(expected) {
            return Vec::new();
        }
        let provider = string(&message["provider"]);
        let model = string(&message["model"]);
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
        self.model_option = invocation.args.model.clone();
        self.expected = match &invocation.args.model {
            Some(model) => Some(parse_model(model)?),
            None => None,
        };
        let user_dir = user_state_dir(&invocation.session, &invocation.cwd);
        let state = match user_dir.as_deref() {
            Some(dir) if invocation.args.dry_run => dir
                .is_dir()
                .then(|| state(dir).map_err(|error| read_error(dir, &error)))
                .transpose()?,
            Some(dir) => Some(prepare_state(dir)?),
            None => None,
        };
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
        let mut env = Vec::new();
        let mut service_hosts = Vec::new();
        if let Some(proxy) = &invocation.proxy {
            let provider = service_provider(self.expected.as_ref(), user_dir.as_deref());
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
            env.extend(proxy_environment(&proxy.port_text()));
        }
        let wrapped = sandbox::wrap(
            &invocation.sandbox,
            &invocation.cwd,
            &invocation.tempdir,
            state.as_ref(),
            invocation.proxy.as_ref(),
            invocation.args.dry_run,
        )?;
        argv = wrapped.argv(&argv);
        self.sandbox = wrapped.kind;
        Ok(Launch {
            argv,
            stdin: Vec::new(),
            env,
            signal_wrapped_child: wrapped.kind == SandboxKind::Bubblewrap,
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
        if let Some(detail) = wrapper_failure(self.sandbox, exit_code, stderr_tail) {
            return Some(detail);
        }
        if exit_code != Some(0) {
            return Some(String::new());
        }
        match &self.last_stop {
            Some(stop) if FAILED_STOP_REASONS.contains(&stop.reason.as_str()) => {
                Some(match &stop.error_message {
                    Some(message) => detail_head(message),
                    None => format!("stopReason: {}", stop.reason),
                })
            }
            Some(_) => None,
            None => Some(NO_REPLY_DETAIL.to_string()),
        }
    }
}

pub(crate) fn parse_model(value: &str) -> Result<Model, String> {
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

pub(crate) fn user_state_dir(session: &Session, cwd: &Path) -> Option<PathBuf> {
    session.runtime_dir(cwd, STATE_DIR_VARIABLE, STATE_HOME_SUBDIR)
}

pub(crate) fn prepare_state(user_dir: &Path) -> Result<PiState, String> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(user_dir)
        .map_err(|error| {
            format!(
                "cannot create the pi state directory {}: {error}",
                user_dir.display()
            )
        })?;
    state(user_dir).map_err(|error| read_error(user_dir, &error))
}

fn read_error(user_dir: &Path, error: &std::io::Error) -> String {
    format!(
        "cannot read the pi state directory {}: {error}",
        user_dir.display()
    )
}

pub fn state(user_dir: &Path) -> std::io::Result<PiState> {
    let dir = std::fs::canonicalize(user_dir)?;
    let login = dir.join(LOGIN_FILE);
    let login_target = std::fs::canonicalize(&login)
        .ok()
        .filter(|target| *target != login);
    let mut readonly = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if WRITABLE_STATE_ENTRIES
            .iter()
            .any(|name| entry.file_name() == *name)
        {
            continue;
        }
        let path = entry.path();
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir() || metadata.is_file()) {
            readonly.push(path);
        }
    }
    readonly.sort();
    Ok(PiState {
        writable: WRITABLE_STATE_ENTRIES
            .iter()
            .map(|name| dir.join(name))
            .collect(),
        dir,
        login_target,
        readonly,
    })
}

pub(crate) fn default_model(settings: &Path) -> Option<Model> {
    Some(Model {
        provider: settings_string(settings, DEFAULT_PROVIDER_KEY)?,
        model: settings_string(settings, DEFAULT_MODEL_KEY)?,
    })
}

pub(crate) fn service_provider(
    expected: Option<&Model>,
    user_dir: Option<&Path>,
) -> Option<String> {
    match expected {
        Some(model) => Some(model.provider.clone()),
        None => {
            user_dir.and_then(|dir| settings_string(&dir.join(SETTINGS_FILE), DEFAULT_PROVIDER_KEY))
        }
    }
}

pub(crate) fn assistant_model(message: &Value) -> Option<Model> {
    if message["role"] != "assistant" {
        return None;
    }
    Some(Model {
        provider: message["provider"].as_str()?.to_string(),
        model: message["model"].as_str()?.to_string(),
    })
}

pub(crate) fn service_host(provider: &str) -> Option<&'static str> {
    SERVICE_HOSTS
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|(_, host)| *host)
}

fn settings_string(settings: &Path, key: &str) -> Option<String> {
    let bytes = std::fs::read(settings).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get(key)?.as_str().map(str::to_string)
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
        "bash" => args["command"]
            .as_str()
            .map(|command| first_line(command).to_string()),
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
    use crate::cli::{Cli, Format, Parsed, SandboxMode};
    use crate::network::ProxyEndpoint;
    use crate::sandbox::{Sandbox, Wrapper};
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

        fn real_user_state(&self) -> PathBuf {
            std::fs::canonicalize(self.user_state()).unwrap()
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
            let (runtime, args) = match Cli::try_parse_from(args).unwrap().command.into_parsed() {
                Parsed::Run(runtime, args) => (runtime, args),
                _ => unreachable!("the tests parse runtime subcommands"),
            };
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
                    wrapper: if sandboxed {
                        Wrapper::Bubblewrap {
                            bwrap: PathBuf::from("/usr/bin/bwrap"),
                            socat: PathBuf::from("/usr/bin/socat"),
                        }
                    } else {
                        Wrapper::None
                    },
                    reason: String::new(),
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
        assert_eq!(launch.env, vec![]);
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

    fn mounts(argv: &[String]) -> Vec<(&str, &str)> {
        let separator = argv.iter().position(|arg| arg == "--dev").unwrap();
        argv[1..separator]
            .chunks(3)
            .map(|chunk| {
                assert_eq!(chunk[1], chunk[2]);
                (chunk[0].as_str(), chunk[1].as_str())
            })
            .collect()
    }

    #[test]
    fn sandboxed_argv_binds_the_state_dir_and_the_real_login_file() {
        let setup = Setup::new();
        let real = setup.write_user_file("real-auth.json", "{}");
        std::os::unix::fs::symlink(&real, setup.user_state().join("auth.json")).unwrap();
        setup.write_user_file("settings.json", "{}");
        let home = setup.with_home();
        let launch = launch(&setup, &pairs(&home), true, &[]);
        assert!(launch.signal_wrapped_child);
        let state = setup.real_user_state();
        let text = |path: &Path| path.to_string_lossy().into_owned();
        assert_eq!(
            mounts(&launch.argv),
            [
                ("--ro-bind", "/"),
                ("--bind", text(&setup.work()).as_str()),
                ("--bind", text(&setup.tempdir()).as_str()),
                ("--bind", text(&state).as_str()),
                ("--ro-bind", text(&state.join("real-auth.json")).as_str()),
                ("--ro-bind", text(&state.join("settings.json")).as_str()),
                ("--bind", text(&state.join("real-auth.json")).as_str()),
            ]
        );
        assert_eq!(launch.argv[0], "/usr/bin/bwrap");
        let separator = launch.argv.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(launch.argv[separator + 1], "/opt/bin/pi");
    }

    #[test]
    fn sandboxed_argv_without_a_state_dir_binds_only_cwd_and_tempdir() {
        let setup = Setup::new();
        let launch = launch(&setup, &[], true, &[]);
        assert_eq!(launch.argv.iter().filter(|arg| *arg == "--bind").count(), 2);
        assert!(!setup.user_state().exists());
    }

    #[test]
    fn launch_creates_the_missing_state_dir_with_private_permissions() {
        let setup = Setup::new();
        let home = setup.with_home();
        let launch = launch(&setup, &pairs(&home), true, &[]);
        assert_eq!(mode(&setup.home().join(".pi")), 0o700);
        assert_eq!(mode(&setup.user_state()), 0o700);
        assert_eq!(std::fs::read_dir(setup.user_state()).unwrap().count(), 0);
        let state = setup.real_user_state();
        assert_eq!(
            mounts(&launch.argv)[3..],
            [("--bind", state.to_str().unwrap())]
        );
        assert_eq!(launch.env, vec![]);
    }

    #[test]
    fn state_lists_readonly_entries_sorted_without_writable_or_dangling_ones() {
        let setup = Setup::new();
        for name in [
            "auth.json",
            "auth.json.lock",
            "models-store.json",
            "models-store.json.lock",
            "settings.json.lock",
            "settings.json",
            "models.json",
            "bin/fd",
            "extensions/x.js",
        ] {
            setup.write_user_file(name, "x");
        }
        let dir = setup.user_state();
        std::os::unix::fs::symlink(dir.join("absent"), dir.join("dangling")).unwrap();
        std::os::unix::fs::symlink(dir.join("bin"), dir.join("bin-link")).unwrap();
        std::os::unix::net::UnixListener::bind(dir.join("socket")).unwrap();
        let state = state(&dir).unwrap();
        let real = setup.real_user_state();
        assert_eq!(state.dir, real);
        assert_eq!(
            state.writable,
            [
                real.join("auth.json"),
                real.join("auth.json.lock"),
                real.join("models-store.json"),
                real.join("models-store.json.lock"),
                real.join("settings.json.lock"),
            ]
        );
        assert_eq!(state.login_target, None);
        assert_eq!(
            state.readonly,
            [
                real.join("bin"),
                real.join("bin-link"),
                real.join("extensions"),
                real.join("models.json"),
                real.join("settings.json"),
            ]
        );
    }

    #[test]
    fn state_login_target_is_set_only_for_a_symlinked_login_file() {
        let setup = Setup::new();
        let elsewhere = setup.root.path().join("elsewhere/auth.json");
        std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        std::fs::write(&elsewhere, "{}").unwrap();
        std::fs::create_dir_all(setup.user_state()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, setup.user_state().join("auth.json")).unwrap();
        let linked = state(&setup.user_state()).unwrap();
        assert_eq!(
            linked.login_target,
            Some(std::fs::canonicalize(&elsewhere).unwrap())
        );
        assert_eq!(linked.readonly, Vec::<PathBuf>::new());

        std::fs::remove_file(setup.user_state().join("auth.json")).unwrap();
        assert_eq!(state(&setup.user_state()).unwrap().login_target, None);
        setup.write_user_file("auth.json", "{}");
        assert_eq!(state(&setup.user_state()).unwrap().login_target, None);
    }

    #[test]
    fn settings_string_reads_only_string_values() {
        let setup = Setup::new();
        for content in ["not json", "[1,2]", "\"text\"", ""] {
            let path = setup.write_user_file("settings.json", content);
            assert_eq!(settings_string(&path, "defaultProvider"), None, "{content}");
        }
        assert_eq!(
            settings_string(&setup.user_state().join("missing.json"), "defaultProvider"),
            None
        );
        let path = setup.write_user_file(
            "settings.json",
            "{\"defaultModel\": 3, \"defaultProvider\": \"deepseek\", \"theme\": \"dark\"}",
        );
        assert_eq!(
            settings_string(&path, "defaultProvider"),
            Some("deepseek".to_string())
        );
        assert_eq!(settings_string(&path, "defaultModel"), None);
    }

    #[test]
    fn service_provider_prefers_the_expected_model_then_the_settings() {
        let setup = Setup::new();
        let dir = setup.user_state();
        let expected = Model {
            provider: "deepseek".to_string(),
            model: "deepseek-flash".to_string(),
        };
        setup.write_user_file("settings.json", r#"{"defaultProvider":"openai"}"#);
        assert_eq!(
            service_provider(Some(&expected), Some(&dir)),
            Some("deepseek".to_string())
        );
        assert_eq!(
            service_provider(Some(&expected), None),
            Some("deepseek".to_string())
        );
        assert_eq!(
            service_provider(None, Some(&dir)),
            Some("openai".to_string())
        );
        assert_eq!(service_provider(None, None), None);
        for content in [
            "not json",
            r#"{"defaultModel":"m"}"#,
            r#"{"defaultProvider":5}"#,
        ] {
            setup.write_user_file("settings.json", content);
            assert_eq!(service_provider(None, Some(&dir)), None, "{content}");
        }
        std::fs::remove_file(dir.join("settings.json")).unwrap();
        assert_eq!(service_provider(None, Some(&dir)), None);
    }

    #[test]
    fn assistant_model_needs_an_assistant_with_string_fields() {
        assert_eq!(
            assistant_model(
                &json!({"role": "assistant", "provider": "deepseek", "model": "deepseek-flash"})
            ),
            Some(Model {
                provider: "deepseek".to_string(),
                model: "deepseek-flash".to_string(),
            })
        );
        for message in [
            json!({"role": "user", "provider": "deepseek", "model": "deepseek-flash"}),
            json!({"role": "assistant", "model": "deepseek-flash"}),
            json!({"role": "assistant", "provider": "deepseek"}),
            json!({"role": "assistant", "provider": 1, "model": "deepseek-flash"}),
            json!({"role": "assistant", "provider": "deepseek", "model": null}),
        ] {
            assert_eq!(assistant_model(&message), None, "{message}");
        }
    }

    #[test]
    fn state_dir_variable_from_env_option_selects_the_user_dir() {
        let setup = Setup::new();
        let custom = setup.root.path().join("custom-state");
        std::fs::create_dir(&custom).unwrap();
        std::fs::write(custom.join("auth.json"), "{}").unwrap();
        let home = setup.with_home();
        let variable = format!("PI_CODING_AGENT_DIR={}", custom.display());
        let launch = launch(&setup, &pairs(&home), true, &["--env", &variable]);
        let real = std::fs::canonicalize(&custom).unwrap();
        assert_eq!(
            mounts(&launch.argv)[3..],
            [("--bind", real.to_str().unwrap())]
        );
        assert!(!setup.user_state().exists());
    }

    fn proxy(port: Option<u16>, setup: &Setup) -> ProxyEndpoint {
        ProxyEndpoint {
            port,
            socket: setup.tempdir().join("proxy.sock"),
        }
    }

    fn proxied(setup: &Setup, port: Option<u16>, extra: &[&str]) -> Result<Launch, String> {
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
        let mut expected = Vec::new();
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
        assert!(!setup.user_state().exists());
    }

    fn seatbelt(setup: &Setup, port: Option<u16>, extra: &[&str]) -> (Pi, Launch) {
        let _ = std::fs::remove_file(setup.tempdir().join("seatbelt.sb"));
        let mut invocation = setup.invocation(&pairs(&setup.with_home()), false, extra);
        invocation.sandbox.wrapper = Wrapper::Seatbelt;
        invocation.proxy = Some(proxy(port, setup));
        let mut adapter = Pi::new();
        let launch = adapter
            .launch(Path::new("/opt/bin/pi"), &invocation)
            .unwrap();
        (adapter, launch)
    }

    #[test]
    fn seatbelt_launch_writes_the_profile_and_wraps_pi_directly() {
        let setup = Setup::new();
        let real = setup.write_user_file("real-auth.json", "{}");
        std::os::unix::fs::symlink(&real, setup.user_state().join("auth.json")).unwrap();
        let (_, launch) = seatbelt(&setup, Some(41234), &["--model", "deepseek/deepseek-flash"]);
        let profile_path = setup.tempdir().join("seatbelt.sb");
        let mut expected = vec![
            "/usr/bin/sandbox-exec".to_string(),
            "-f".to_string(),
            profile_path.to_string_lossy().into_owned(),
        ];
        expected.extend(fixed("/opt/bin/pi"));
        expected.extend(["--model", "deepseek/deepseek-flash", "--", "hi"].map(str::to_string));
        assert_eq!(launch.argv, expected);
        assert!(!launch.signal_wrapped_child);
        assert_eq!(launch.service_hosts, ["api.deepseek.com"]);
        assert!(launch.env.contains(&(
            OsString::from("HTTPS_PROXY"),
            OsString::from("http://127.0.0.1:41234")
        )));
        assert_eq!(mode(&profile_path), 0o600);
        let real_path = |path: &Path| std::fs::canonicalize(path).unwrap();
        let state = PiState {
            dir: setup.real_user_state(),
            writable: WRITABLE_STATE_ENTRIES
                .iter()
                .map(|name| setup.real_user_state().join(name))
                .collect(),
            login_target: Some(real_path(&real)),
            readonly: vec![setup.real_user_state().join("real-auth.json")],
        };
        let profile = std::fs::read_to_string(&profile_path).unwrap();
        assert_eq!(
            profile,
            crate::sandbox::seatbelt_profile(
                &real_path(&setup.work()),
                &real_path(&setup.tempdir()),
                Some(&state),
                Some("41234"),
            )
        );
        assert!(profile.contains("/agent/auth.json.lock\")\n"), "{profile}");
        assert!(profile.contains("/agent/real-auth.json\")\n"), "{profile}");
    }

    #[test]
    fn seatbelt_launch_without_a_state_dir_leaves_it_out() {
        let setup = Setup::new();
        let mut invocation = setup.invocation(&[], false, &["--model", "deepseek/deepseek-flash"]);
        invocation.sandbox.wrapper = Wrapper::Seatbelt;
        invocation.proxy = Some(proxy(Some(1), &setup));
        Pi::new()
            .launch(Path::new("/opt/bin/pi"), &invocation)
            .unwrap();
        let profile = std::fs::read_to_string(setup.tempdir().join("seatbelt.sb")).unwrap();
        assert!(!profile.contains("auth.json"), "{profile}");
        assert!(
            profile.ends_with("(remote tcp \"localhost:1\"))\n"),
            "{profile}"
        );
    }

    #[test]
    fn seatbelt_dry_run_writes_no_profile() {
        let setup = Setup::new();
        let (_, launch) = seatbelt(
            &setup,
            None,
            &["--dry-run", "--model", "deepseek/deepseek-flash"],
        );
        assert_eq!(
            launch.argv[..3],
            [
                "/usr/bin/sandbox-exec",
                "-f",
                setup.tempdir().join("seatbelt.sb").to_str().unwrap()
            ]
        );
        assert!(!setup.tempdir().join("seatbelt.sb").exists());
        assert!(!setup.user_state().exists());
    }

    #[test]
    fn seatbelt_profile_that_cannot_be_written_is_a_launch_error() {
        let setup = Setup::new();
        std::fs::write(setup.tempdir().join("seatbelt.sb"), "taken").unwrap();
        let mut invocation = setup.invocation(&[], false, &["--model", "deepseek/deepseek-flash"]);
        invocation.sandbox.wrapper = Wrapper::Seatbelt;
        let detail = Pi::new()
            .launch(Path::new("/opt/bin/pi"), &invocation)
            .unwrap_err();
        assert!(
            detail.starts_with(&format!(
                "cannot write the sandbox profile {}: ",
                setup.tempdir().join("seatbelt.sb").display()
            )),
            "{detail}"
        );
    }

    #[test]
    fn seatbelt_failure_uses_the_sandbox_exec_prefix() {
        let setup = Setup::new();
        let (adapter, _) = seatbelt(&setup, Some(1), &["--model", "deepseek/deepseek-flash"]);
        assert_eq!(
            adapter.failure(Some(65), "sandbox-exec: syntax error\n"),
            Some("sandbox failed to start: syntax error".to_string())
        );
        assert_eq!(
            adapter.failure(Some(1), "bwrap: not on macOS\n"),
            Some(String::new())
        );
    }

    #[test]
    fn dry_run_creates_no_state_dir_and_binds_an_existing_one() {
        let setup = Setup::new();
        let home = setup.with_home();
        let missing = launch(&setup, &pairs(&home), true, &["--dry-run"]);
        assert!(!setup.user_state().exists());
        assert_eq!(
            missing.argv.iter().filter(|arg| *arg == "--bind").count(),
            2
        );
        assert_eq!(missing.env, vec![]);

        setup.write_user_file("auth.json", "{}");
        let existing = launch(&setup, &pairs(&home), true, &["--dry-run"]);
        let state = setup.real_user_state();
        assert_eq!(
            mounts(&existing.argv)[3..],
            [("--bind", state.to_str().unwrap())]
        );
    }

    #[test]
    fn state_dir_that_cannot_be_created_is_a_launch_error() {
        let setup = Setup::new();
        std::fs::write(setup.home(), "a file").unwrap();
        let home = setup.with_home();
        let detail = Pi::new()
            .launch(
                Path::new("/opt/bin/pi"),
                &setup.invocation(&pairs(&home), false, &[]),
            )
            .unwrap_err();
        assert!(
            detail.starts_with(&format!(
                "cannot create the pi state directory {}: ",
                setup.user_state().display()
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
