use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde::Serialize;

#[derive(Parser, Debug)]
#[command(
    name = "agentrun",
    version,
    about,
    arg_required_else_help = false,
    disable_help_subcommand = true,
    after_help = "Documentation: https://yhnocoder.github.io/agentrun/"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(about = "Run claude-code in the sandbox", after_help = RUN_AFTER_HELP)]
    ClaudeCode(RunArgs),
    #[command(about = "Run codex in the sandbox", after_help = RUN_AFTER_HELP)]
    Codex(RunArgs),
    #[command(about = "Run pi in the sandbox", after_help = RUN_AFTER_HELP)]
    Pi(RunArgs),
    #[command(about = "Run dsh (DeepSeek Harness) in the sandbox", after_help = RUN_AFTER_HELP)]
    Dsh(RunArgs),
    #[command(about = "Check whether the runtimes can be used in this environment")]
    Doctor(DoctorArgs),
    #[command(hide = true)]
    DoctorConnect(ConnectArgs),
}

const RUN_AFTER_HELP: &str = "Exit codes: 0 finished, 1 failed, 2 rejected before start, 3 timeout, 4 limit, 130 or 143 interrupted\nDocumentation: https://yhnocoder.github.io/agentrun/pages/guide.html";

const PROMPT: &str = "Prompt";
const MODEL_AND_LIMITS: &str = "Model and limits";
const ISOLATION: &str = "Sandbox, network and reads";
const ENVIRONMENT: &str = "Session environment";
const SAVED_SESSIONS: &str = "Saved sessions";
const OUTPUT: &str = "Output";

pub enum Parsed {
    Run(Runtime, Box<RunArgs>),
    Doctor(DoctorArgs),
    Connect(ConnectArgs),
}

impl Command {
    pub fn into_parsed(self) -> Parsed {
        match self {
            Command::ClaudeCode(args) => Parsed::Run(Runtime::ClaudeCode, Box::new(args)),
            Command::Codex(args) => Parsed::Run(Runtime::Codex, Box::new(args)),
            Command::Pi(args) => Parsed::Run(Runtime::Pi, Box::new(args)),
            Command::Dsh(args) => Parsed::Run(Runtime::Dsh, Box::new(args)),
            Command::Doctor(args) => Parsed::Doctor(args),
            Command::DoctorConnect(args) => Parsed::Connect(args),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
pub enum Runtime {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "pi")]
    Pi,
    #[serde(rename = "dsh")]
    Dsh,
}

impl Runtime {
    pub fn name(self) -> &'static str {
        match self {
            Runtime::ClaudeCode => "claude-code",
            Runtime::Codex => "codex",
            Runtime::Pi => "pi",
            Runtime::Dsh => "dsh",
        }
    }

    pub fn executable(self) -> &'static str {
        match self {
            Runtime::ClaudeCode => "claude",
            Runtime::Codex => "codex",
            Runtime::Pi => "pi",
            Runtime::Dsh => "dsh",
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    #[arg(
        long,
        value_name = "TEXT",
        conflicts_with = "prompt_file",
        allow_hyphen_values = true,
        help_heading = PROMPT,
        help = "Prompt text; without --prompt or --prompt-file the prompt is read from standard input"
    )]
    pub prompt: Option<String>,
    #[arg(
        long,
        value_name = "FILE",
        help_heading = PROMPT,
        help = "Read the prompt from a file"
    )]
    pub prompt_file: Option<PathBuf>,
    #[arg(
        long,
        value_name = "FILE",
        help_heading = PROMPT,
        help = "Image given to the model with the prompt (png, jpeg, gif, webp), repeatable"
    )]
    pub image: Vec<PathBuf>,
    #[arg(
        long,
        value_name = "DIR",
        help_heading = PROMPT,
        help = "Working directory of the session (default: current directory)"
    )]
    pub cwd: Option<PathBuf>,
    #[arg(
        long,
        value_name = "ID",
        help_heading = MODEL_AND_LIMITS,
        help = "Model, passed to the runtime as is; pi needs provider/model"
    )]
    pub model: Option<String>,
    #[arg(
        long,
        value_name = "LEVEL",
        help_heading = MODEL_AND_LIMITS,
        help = "Reasoning effort, passed to the runtime as is"
    )]
    pub effort: Option<String>,
    #[arg(
        long,
        value_name = "N",
        value_parser = positive,
        help_heading = MODEL_AND_LIMITS,
        help = "Stop the runtime when the main agent starts model call N+1 (claude-code, pi and dsh)"
    )]
    pub max_turns: Option<u64>,
    #[arg(
        long,
        value_name = "N",
        value_parser = positive,
        help_heading = MODEL_AND_LIMITS,
        help = "Stop the runtime when the tokens of all model calls exceed N (claude-code, pi and dsh)"
    )]
    pub max_tokens: Option<u64>,
    #[arg(
        long,
        value_name = "SECONDS",
        value_parser = positive,
        help_heading = MODEL_AND_LIMITS,
        help = "Stop the runtime after this many seconds"
    )]
    pub timeout: Option<u64>,
    #[arg(
        long,
        help_heading = MODEL_AND_LIMITS,
        help = "Do not allow the agent to start subagents"
    )]
    pub no_subagents: bool,
    #[arg(
        long,
        value_enum,
        help_heading = ISOLATION,
        help = "Sandbox mode (default: AGENTRUN_SANDBOX, or on)"
    )]
    pub sandbox: Option<SandboxMode>,
    #[arg(
        long,
        value_enum,
        default_value_t = NetworkMode::None,
        help_heading = ISOLATION,
        help = "Which hosts the agent's commands may reach"
    )]
    pub network: NetworkMode,
    #[arg(
        long,
        value_name = "HOST[:PORT]",
        help_heading = ISOLATION,
        help = "Host allowed with --network custom, repeatable; *.example.com matches subdomains"
    )]
    pub allow_host: Vec<String>,
    #[arg(
        long,
        value_name = "PATH",
        help_heading = ISOLATION,
        help = "File or directory the agent may not read, repeatable, added to the built-in list"
    )]
    pub deny_read: Vec<PathBuf>,
    #[arg(
        long,
        help_heading = ISOLATION,
        help = "Do not deny reading the built-in list of credential paths"
    )]
    pub no_default_deny_read: bool,
    #[arg(
        long,
        value_name = "DIR",
        help_heading = ENVIRONMENT,
        help = "Directory prepended to the session PATH, repeatable"
    )]
    pub path: Vec<PathBuf>,
    #[arg(
        long,
        value_name = "KEY[=VALUE]",
        help_heading = ENVIRONMENT,
        help = "Environment variable for the session, repeatable; KEY alone takes the value from your environment"
    )]
    pub env: Vec<String>,
    #[arg(
        long,
        value_name = "FILE",
        help_heading = ENVIRONMENT,
        help = "Read environment variables from a file, repeatable"
    )]
    pub env_file: Vec<PathBuf>,
    #[arg(
        long,
        value_name = "DIR",
        help_heading = SAVED_SESSIONS,
        help = "Save the session in this directory, outside --cwd, so that --resume can continue it"
    )]
    pub session_dir: Option<PathBuf>,
    #[arg(
        long,
        value_name = "ID",
        requires = "session_dir",
        help_heading = SAVED_SESSIONS,
        help = "Continue the session with this full id saved in --session-dir"
    )]
    pub resume: Option<String>,
    #[arg(
        long,
        value_enum,
        help_heading = OUTPUT,
        help = "Format of standard output (default: rich on a terminal, otherwise jsonl)"
    )]
    pub format: Option<Format>,
    #[arg(
        long,
        value_name = "FILE",
        help_heading = OUTPUT,
        help = "Write the raw output of the runtime to this file"
    )]
    pub raw: Option<PathBuf>,
    #[arg(
        long,
        help_heading = OUTPUT,
        help = "Print diagnostics to standard error and keep the session temporary directory"
    )]
    pub debug: bool,
    #[arg(
        long,
        help_heading = OUTPUT,
        help = "Run the checks and print the command without running it"
    )]
    pub dry_run: bool,
    #[arg(
        last = true,
        value_name = "ARGS",
        help = "Arguments passed to the runtime after the ones agentrun sets; they can override the sandbox settings"
    )]
    pub runtime_args: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct DoctorArgs {
    #[arg(
        value_enum,
        value_name = "RUNTIME",
        help = "Runtimes to check, in the order claude-code, codex, pi, dsh (default: all)"
    )]
    pub runtimes: Vec<Runtime>,
    #[arg(
        long,
        value_name = "ID",
        help = "Model; used by the pi and dsh checks, ignored for claude-code and codex"
    )]
    pub model: Option<String>,
    #[arg(
        long,
        value_enum,
        help = "Sandbox mode (default: AGENTRUN_SANDBOX, or on)"
    )]
    pub sandbox: Option<SandboxMode>,
    #[arg(
        long,
        value_enum,
        default_value_t = NetworkMode::None,
        help = "Which hosts the agent's commands may reach; the check runs only for full and custom"
    )]
    pub network: NetworkMode,
    #[arg(
        long,
        value_name = "HOST[:PORT]",
        help = "Host allowed with --network custom, repeatable"
    )]
    pub allow_host: Vec<String>,
    #[arg(
        long,
        value_name = "DIR",
        help = "Directory prepended to the session PATH, repeatable"
    )]
    pub path: Vec<PathBuf>,
    #[arg(
        long,
        value_name = "KEY[=VALUE]",
        help = "Environment variable for the session, repeatable"
    )]
    pub env: Vec<String>,
    #[arg(
        long,
        value_name = "FILE",
        help = "Read environment variables from a file, repeatable"
    )]
    pub env_file: Vec<PathBuf>,
    #[arg(long, help = "Print one JSON array instead of one line per check")]
    pub json: bool,
    #[arg(
        long,
        help = "Print each command to standard error and keep the temporary directory"
    )]
    pub debug: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ConnectArgs {
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SandboxMode {
    #[value(help = "Refuse to run when the sandbox cannot start")]
    On,
    #[value(help = "Run without the sandbox when it cannot start")]
    Relax,
    #[value(help = "Run without the sandbox, for example in a docker container")]
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    #[value(help = "No network for commands; the runtime still reaches its model service")]
    None,
    #[value(help = "Any public host, and web search")]
    Full,
    #[value(help = "Only the hosts given with --allow-host")]
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    #[value(help = "Lines with a status bar, for a terminal")]
    Rich,
    #[value(help = "One line per event, for logs")]
    Text,
    #[value(help = "One JSON event per line, for programs")]
    Jsonl,
}

fn positive(value: &str) -> Result<u64, String> {
    match value.parse::<u64>() {
        Ok(number) if number > 0 => Ok(number),
        _ => Err("expected a positive integer".to_string()),
    }
}

pub(crate) fn prescan_format(args: &[OsString]) -> Option<Format> {
    let mut rest = args.iter().skip(1).map(|arg| arg.to_str());
    while let Some(arg) = rest.next() {
        match arg {
            Some("--") => return None,
            Some("--format") => return rest.next().flatten().and_then(parse_format),
            Some(arg) => {
                if let Some(value) = arg.strip_prefix("--format=") {
                    return parse_format(value);
                }
            }
            None => {}
        }
    }
    None
}

pub(crate) fn option_after_prompt(args: &[OsString]) -> Option<String> {
    let runtime = Runtime::from_str(args.get(1)?.to_str()?, false).ok()?;
    let command = Cli::command();
    let subcommand = command.find_subcommand(runtime.name())?;
    let options: Vec<String> = subcommand
        .get_arguments()
        .filter_map(|arg| arg.get_long())
        .chain(["help"])
        .map(|long| format!("--{long}"))
        .collect();
    let mut rest = args.iter().skip(1).map(|arg| arg.to_str());
    while let Some(arg) = rest.next() {
        match arg {
            Some("--") => return None,
            Some("--prompt") => {
                let value = rest.next().flatten()?;
                if value == "--" {
                    return Some(
                        "--prompt is followed by --; to use it as the prompt, write --prompt=--"
                            .to_string(),
                    );
                }
                let given = value.split_once('=').map_or(value, |(name, _)| name);
                if options.iter().any(|option| option == given) {
                    return Some(format!(
                        "--prompt is followed by the option {given}; to use it as the prompt, write --prompt={value}"
                    ));
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_format(value: &str) -> Option<Format> {
    Format::from_str(value, false).ok()
}

pub(crate) fn default_format(stdout_is_terminal: bool) -> Format {
    if stdout_is_terminal {
        Format::Rich
    } else {
        Format::Jsonl
    }
}

pub(crate) const MISSING_COMMAND: &str =
    "no command given: use claude-code, codex, pi, dsh or doctor; see agentrun --help";

pub(crate) fn usage_error_detail(rendered: &str) -> String {
    let mut joined = String::new();
    let mut in_list = false;
    for line in rendered
        .lines()
        .take_while(|line| !line.starts_with("Usage:") && !line.starts_with("For more information"))
    {
        let text = line.trim();
        if text.is_empty() {
            in_list = false;
            continue;
        }
        let indented = line.starts_with(char::is_whitespace);
        if !joined.is_empty() {
            joined.push_str(if joined.ends_with(':') {
                in_list = indented;
                " "
            } else if in_list && indented {
                ", "
            } else {
                in_list = false;
                "; "
            });
        }
        joined.push_str(text);
    }
    joined
        .strip_prefix("error: ")
        .map(str::to_string)
        .unwrap_or(joined)
}

#[cfg(test)]
pub(crate) fn parse_run(argv: &[&str]) -> (Runtime, RunArgs) {
    match Cli::try_parse_from(argv).unwrap().command.into_parsed() {
        Parsed::Run(runtime, args) => (runtime, *args),
        _ => unreachable!("the tests parse runtime subcommands"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn prescan_finds_separate_value() {
        let found = prescan_format(&args(&["agentrun", "pi", "--format", "text", "--bogus"]));
        assert_eq!(found, Some(Format::Text));
    }

    #[test]
    fn prescan_finds_equals_value() {
        let found = prescan_format(&args(&["agentrun", "pi", "--format=jsonl"]));
        assert_eq!(found, Some(Format::Jsonl));
    }

    #[test]
    fn prescan_ignores_invalid_value() {
        assert_eq!(
            prescan_format(&args(&["agentrun", "pi", "--format", "xml"])),
            None
        );
        assert_eq!(prescan_format(&args(&["agentrun", "pi", "--format"])), None);
    }

    #[test]
    fn prescan_stops_at_separator() {
        let found = prescan_format(&args(&["agentrun", "pi", "--", "--format", "text"]));
        assert_eq!(found, None);
    }

    #[test]
    fn default_format_depends_on_terminal() {
        assert_eq!(default_format(true), Format::Rich);
        assert_eq!(default_format(false), Format::Jsonl);
    }

    #[test]
    fn usage_error_detail_continues_a_line_that_ends_with_a_colon() {
        let rendered = "error: the following required arguments were not provided:\n  --session-dir <DIR>\n\nUsage: agentrun pi --session-dir <DIR> --resume <ID>\n\nFor more information, try '--help'.\n";
        assert_eq!(
            usage_error_detail(rendered),
            "the following required arguments were not provided: --session-dir <DIR>"
        );
    }

    #[test]
    fn usage_error_detail_joins_listed_items_with_commas() {
        let rendered = "error: the following required arguments were not provided:\n  --host <HOST>\n  --port <PORT>\n\nUsage: agentrun doctor-connect --host <HOST> --port <PORT>\n\nFor more information, try '--help'.\n";
        assert_eq!(
            usage_error_detail(rendered),
            "the following required arguments were not provided: --host <HOST>, --port <PORT>"
        );
        let rendered = "error: unexpected argument '--promt' found\n\n  tip: a similar argument exists: '--prompt'\n  tip: to pass '--promt' as a value, use '-- --promt'\n\nUsage: agentrun pi [OPTIONS]\n";
        assert_eq!(
            usage_error_detail(rendered),
            "unexpected argument '--promt' found; tip: a similar argument exists: '--prompt'; tip: to pass '--promt' as a value, use '-- --promt'"
        );
    }

    #[test]
    fn usage_error_detail_is_one_line() {
        let rendered = "error: unexpected argument '--bogus' found\n\n  tip: to pass '--bogus' as a value, use '-- --bogus'\n\nUsage: agentrun pi [OPTIONS]\n\nFor more information, try '--help'.\n";
        assert_eq!(
            usage_error_detail(rendered),
            "unexpected argument '--bogus' found; tip: to pass '--bogus' as a value, use '-- --bogus'"
        );
    }
}
