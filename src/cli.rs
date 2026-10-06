use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

#[derive(Parser, Debug)]
#[command(
    name = "agentrun",
    version,
    about,
    arg_required_else_help = false,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(about = "Run claude-code")]
    ClaudeCode(RunArgs),
    #[command(about = "Run codex")]
    Codex(RunArgs),
    #[command(about = "Run pi")]
    Pi(RunArgs),
    #[command(about = "Check whether the runtimes can be used in this environment")]
    Doctor(DoctorArgs),
    #[command(hide = true)]
    DoctorConnect(ConnectArgs),
}

pub enum Parsed {
    Run(Runtime, RunArgs),
    Doctor(DoctorArgs),
    Connect(ConnectArgs),
}

impl Command {
    pub fn into_parsed(self) -> Parsed {
        match self {
            Command::ClaudeCode(args) => Parsed::Run(Runtime::ClaudeCode, args),
            Command::Codex(args) => Parsed::Run(Runtime::Codex, args),
            Command::Pi(args) => Parsed::Run(Runtime::Pi, args),
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
}

impl Runtime {
    pub fn name(self) -> &'static str {
        match self {
            Runtime::ClaudeCode => "claude-code",
            Runtime::Codex => "codex",
            Runtime::Pi => "pi",
        }
    }

    pub fn executable(self) -> &'static str {
        match self {
            Runtime::ClaudeCode => "claude",
            Runtime::Codex => "codex",
            Runtime::Pi => "pi",
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    #[arg(long, value_name = "DIR", help = "Working directory of the session")]
    pub cwd: Option<PathBuf>,
    #[arg(
        long,
        value_name = "TEXT",
        conflicts_with = "prompt_file",
        help = "Prompt text"
    )]
    pub prompt: Option<String>,
    #[arg(long, value_name = "FILE", help = "Read the prompt from a file")]
    pub prompt_file: Option<PathBuf>,
    #[arg(long, value_name = "ID", help = "Model, passed to the runtime as is")]
    pub model: Option<String>,
    #[arg(long, value_name = "LEVEL", help = "Reasoning effort")]
    pub effort: Option<String>,
    #[arg(
        long,
        value_name = "N",
        value_parser = clap::value_parser!(u64).range(1..),
        help = "Maximum number of turns (claude-code only)"
    )]
    pub max_turns: Option<u64>,
    #[arg(
        long,
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u64).range(1..),
        help = "Terminate the runtime after this many seconds"
    )]
    pub timeout: Option<u64>,
    #[arg(
        long,
        value_enum,
        help = "What to do when the sandbox is not available (default: AGENTRUN_SANDBOX, or on)"
    )]
    pub sandbox: Option<SandboxMode>,
    #[arg(long, value_enum, default_value_t = NetworkMode::None, help = "Which hosts commands may reach")]
    pub network: NetworkMode,
    #[arg(long, value_name = "HOST[:PORT]", help = "Allowed host, repeatable")]
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
    #[arg(long, help = "Do not allow the agent to start subagents")]
    pub no_subagents: bool,
    #[arg(long, value_enum, help = "Format of standard output")]
    pub format: Option<Format>,
    #[arg(
        long,
        value_name = "FILE",
        help = "Write the raw output of the runtime to this file"
    )]
    pub raw: Option<PathBuf>,
    #[arg(
        long,
        help = "Print diagnostics to standard error and keep the session temporary directory"
    )]
    pub debug: bool,
    #[arg(long, help = "Run the checks and print the command without running it")]
    pub dry_run: bool,
    #[arg(
        last = true,
        value_name = "ARGS",
        help = "Arguments passed to the runtime"
    )]
    pub runtime_args: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct DoctorArgs {
    #[arg(
        value_enum,
        value_name = "RUNTIME",
        help = "Runtimes to check, in the order claude-code, codex, pi (default: all)"
    )]
    pub runtimes: Vec<Runtime>,
    #[arg(long, value_name = "ID", help = "Model, checked for every runtime")]
    pub model: Option<String>,
    #[arg(
        long,
        value_enum,
        help = "What to do when the sandbox is not available (default: AGENTRUN_SANDBOX, or on)"
    )]
    pub sandbox: Option<SandboxMode>,
    #[arg(long, value_enum, default_value_t = NetworkMode::None, help = "Which hosts commands may reach")]
    pub network: NetworkMode,
    #[arg(long, value_name = "HOST[:PORT]", help = "Allowed host, repeatable")]
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
    On,
    Relax,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    None,
    Full,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Rich,
    Text,
    Jsonl,
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

pub(crate) fn usage_error_detail(rendered: &str) -> String {
    let joined = rendered
        .lines()
        .take_while(|line| !line.starts_with("Usage:") && !line.starts_with("For more information"))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    joined
        .strip_prefix("error: ")
        .map(str::to_string)
        .unwrap_or(joined)
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
    fn usage_error_detail_is_one_line() {
        let rendered = "error: unexpected argument '--bogus' found\n\n  tip: to pass '--bogus' as a value, use '-- --bogus'\n\nUsage: agentrun pi [OPTIONS]\n\nFor more information, try '--help'.\n";
        assert_eq!(
            usage_error_detail(rendered),
            "unexpected argument '--bogus' found tip: to pass '--bogus' as a value, use '-- --bogus'"
        );
    }
}
