use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use agentrun::adapter::Adapter;
use agentrun::process_tree;
use agentrun::run::{Caller, run};
use agentrun::signal::Signals;

use super::env::poll_until;
use super::fake::FakeAdapter;

pub const FAKE_AGENTRUN: &str = "AGENTRUN_TEST_FAKE_AGENTRUN";
pub const READY_FILE: &str = "AGENTRUN_TEST_READY_FILE";
const ROLE: &str = "AGENTRUN_TEST_ROLE";
const SLEEPER_LIFETIME: Duration = Duration::from_secs(60);
const GONE_LIMIT: Duration = Duration::from_secs(2);
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

pub fn take_over_if_spawned() {
    if let Some(role) = std::env::var_os(ROLE) {
        let pidfile = PathBuf::from(std::env::args_os().nth(1).unwrap());
        play_role(role.to_str().unwrap(), &pidfile);
    }
    if std::env::var_os(FAKE_AGENTRUN).is_some() {
        std::process::exit(fake_agentrun().into());
    }
}

fn play_role(role: &str, pidfile: &Path) -> ! {
    match role {
        "detached" => {
            unsafe {
                libc::setsid();
            }
            sleeper(pidfile);
        }
        "sleeper" => sleeper(pidfile),
        "shell" => {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg(pidfile)
                .env(ROLE, "command")
                .process_group(0)
                .status()
                .unwrap();
            std::process::exit(status.code().unwrap_or(1));
        }
        "command" => {
            Command::new(std::env::current_exe().unwrap())
                .arg(pidfile)
                .env(ROLE, "sleeper")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            thread::sleep(Duration::from_millis(20));
            std::process::exit(0);
        }
        other => panic!("unknown role {other}"),
    }
}

fn sleeper(pidfile: &Path) -> ! {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    std::fs::write(pidfile, std::process::id().to_string()).unwrap();
    thread::sleep(SLEEPER_LIFETIME);
    std::process::exit(0);
}

fn fake_agentrun() -> u8 {
    process_tree::claim_orphans();
    let signals = Signals::install();
    if let Some(path) = std::env::var_os(READY_FILE) {
        std::fs::write(path, "").unwrap();
    }
    let caller = Caller {
        args: std::env::args_os().collect(),
        env: std::env::vars_os().collect(),
        stdin: Box::new(std::io::stdin()),
        stdin_is_terminal: std::io::stdin().is_terminal(),
        stdout: Arc::new(Mutex::new(std::io::stdout())),
        stdout_is_terminal: std::io::stdout().is_terminal(),
        stderr: Arc::new(Mutex::new(std::io::stderr())),
        stderr_is_terminal: std::io::stderr().is_terminal(),
        signals,
    };
    run(caller, &|_| {
        Box::new(FakeAdapter::new(false)) as Box<dyn Adapter>
    })
}

pub fn spawn_role(role: &str, pidfile: &str) -> String {
    let exe = std::env::current_exe().unwrap();
    format!(
        "{ROLE}={role} \"{}\" \"{pidfile}\" </dev/null >/dev/null 2>&1 &\nwhile [ ! -s \"{pidfile}\" ]; do sleep 0.05; done\n",
        exe.display()
    )
}

pub fn spawn_lock() -> MutexGuard<'static, ()> {
    SPAWN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn ps(args: &[&str]) -> String {
    let _guard = spawn_lock();
    let output = Command::new("ps").args(args).output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub fn process_is_gone(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return true;
    }
    let state = ps(&["-o", "stat=", "-p", &pid.to_string()]);
    state.is_empty() || state.starts_with('Z')
}

pub fn describe(pid: i32) -> String {
    ps(&["-o", "pid=,ppid=,stat=,args=", "-p", &pid.to_string()])
}

pub fn wait_until_gone(pid: i32) -> bool {
    poll_until(GONE_LIMIT, || process_is_gone(pid))
}

pub fn read_pid(path: &Path) -> i32 {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    text.trim()
        .parse()
        .unwrap_or_else(|error| panic!("{} holds {text:?}: {error}", path.display()))
}
