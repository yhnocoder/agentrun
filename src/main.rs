use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use agentrun::run::{Caller, run};

fn main() -> ExitCode {
    let caller = Caller {
        args: std::env::args_os().collect(),
        env: std::env::vars_os().collect(),
        stdin_is_terminal: std::io::stdin().is_terminal(),
        stdin: Box::new(std::io::stdin()),
        stdout_is_terminal: std::io::stdout().is_terminal(),
        stdout: Box::new(std::io::stdout()),
        stderr: Arc::new(Mutex::new(std::io::stderr())),
    };
    ExitCode::from(run(caller, &|_| None))
}
