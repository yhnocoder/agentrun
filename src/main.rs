use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use agentrun::claim_orphans;
use agentrun::run::{Caller, Signals, run};
use agentrun::runtime;

fn main() -> ExitCode {
    claim_orphans();
    let signals = Signals::install();
    let caller = Caller {
        args: std::env::args_os().collect(),
        env: std::env::vars_os().collect(),
        stdin_is_terminal: std::io::stdin().is_terminal(),
        stdin: Box::new(std::io::stdin()),
        stdout_is_terminal: std::io::stdout().is_terminal(),
        stdout: Arc::new(Mutex::new(std::io::stdout())),
        stderr: Arc::new(Mutex::new(std::io::stderr())),
        stderr_is_terminal: std::io::stderr().is_terminal(),
        signals,
    };
    ExitCode::from(run(caller, &runtime::builtin))
}
