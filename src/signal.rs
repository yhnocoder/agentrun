use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator;

use crate::cli::{Format, SandboxMode};
use crate::event::{Body, End, EndStatus, Event};
use crate::output::Output;
use crate::usage::Usage;

pub const GRACE_PERIOD: Duration = Duration::from_secs(5);
const PROC_ROOT: &str = "/proc";

pub type SharedWriter = Arc<Mutex<dyn Write + Send>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Interrupt,
    Terminate,
}

impl Signal {
    fn from_number(number: i32) -> Option<Signal> {
        match number {
            SIGINT => Some(Signal::Interrupt),
            SIGTERM => Some(Signal::Terminate),
            _ => None,
        }
    }

    pub fn number(self) -> i32 {
        match self {
            Signal::Interrupt => SIGINT,
            Signal::Terminate => SIGTERM,
        }
    }

    pub fn exit_code(self) -> u8 {
        match self {
            Signal::Interrupt => 130,
            Signal::Terminate => 143,
        }
    }
}

pub struct Signals {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    first_signal: Option<Signal>,
    timed_out: bool,
    phase: Phase,
}

enum Phase {
    Starting,
    Preparing(Preparation),
    Doctoring(Doctoring),
    Running(Running),
    Finishing,
}

struct Doctoring {
    check_pid: Option<i32>,
    tempdir: Option<PathBuf>,
    keep_tempdir: bool,
    report: Box<dyn Fn(Signal) + Send>,
}

impl Doctoring {
    fn abort(&self, signal: Signal) -> u8 {
        if let Some(pid) = self.check_pid {
            kill_group(pid, libc::SIGKILL);
            kill_process(pid, libc::SIGKILL);
        }
        if let (Some(tempdir), false) = (&self.tempdir, self.keep_tempdir) {
            let _ = std::fs::remove_dir_all(tempdir);
        }
        (self.report)(signal);
        signal.exit_code()
    }
}

struct Preparation {
    stdout: SharedWriter,
    format: Format,
    started: Instant,
    tempdirs: Vec<PathBuf>,
    keep_tempdir: bool,
    check_pid: Option<i32>,
}

struct Running {
    pgid: i32,
    signal_wrapped_child: bool,
    terminating: bool,
    deadline: Option<Instant>,
}

impl Running {
    fn terminate(&mut self, signal: i32) {
        if self.terminating {
            kill_group(self.pgid, libc::SIGKILL);
            return;
        }
        let child = if self.signal_wrapped_child {
            wrapped_child(Path::new(PROC_ROOT), self.pgid)
        } else {
            None
        };
        match child {
            Some(pid) => kill_process(pid, signal),
            None => kill_group(self.pgid, signal),
        }
        self.terminating = true;
        self.deadline = Some(Instant::now() + GRACE_PERIOD);
    }
}

impl Signals {
    pub fn install() -> Signals {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                first_signal: None,
                timed_out: false,
                phase: Phase::Starting,
            }),
            changed: Condvar::new(),
        });
        let mut signals =
            iterator::Signals::new([SIGINT, SIGTERM]).expect("SIGINT and SIGTERM can be received");
        let receiver = Arc::clone(&shared);
        thread::spawn(move || {
            for number in signals.forever() {
                if let Some(signal) = Signal::from_number(number) {
                    receiver.receive(signal);
                }
            }
        });
        Signals { shared }
    }

    pub fn prepare(
        &self,
        stdout: SharedWriter,
        format: Format,
        started: Instant,
    ) -> Result<(), u8> {
        let mut state = self.shared.lock();
        let preparation = Preparation {
            stdout,
            format,
            started,
            tempdirs: Vec::new(),
            keep_tempdir: false,
            check_pid: None,
        };
        match state.first_signal {
            Some(signal) => {
                state.phase = Phase::Finishing;
                Err(preparation.abort(signal))
            }
            None => {
                state.phase = Phase::Preparing(preparation);
                Ok(())
            }
        }
    }

    pub fn doctoring(&self, report: Box<dyn Fn(Signal) + Send>) -> Result<(), u8> {
        let mut state = self.shared.lock();
        if let Some(signal) = state.first_signal {
            state.phase = Phase::Finishing;
            return Err(signal.exit_code());
        }
        state.phase = Phase::Doctoring(Doctoring {
            check_pid: None,
            tempdir: None,
            keep_tempdir: false,
            report,
        });
        Ok(())
    }

    pub fn format(&self, format: Format) {
        if let Phase::Preparing(preparation) = &mut self.shared.lock().phase {
            preparation.format = format;
        }
    }

    pub fn checking(&self, pid: Option<i32>) {
        match &mut self.shared.lock().phase {
            Phase::Preparing(preparation) => preparation.check_pid = pid,
            Phase::Doctoring(doctoring) => doctoring.check_pid = pid,
            _ => {}
        }
    }

    pub fn tempdir(&self, path: PathBuf, keep: bool) {
        match &mut self.shared.lock().phase {
            Phase::Preparing(preparation) => {
                preparation.tempdirs.push(path);
                preparation.keep_tempdir = keep;
            }
            Phase::Doctoring(doctoring) => {
                doctoring.tempdir = Some(path);
                doctoring.keep_tempdir = keep;
            }
            _ => {}
        }
    }

    pub fn running(&self, pgid: i32, timeout: Option<Duration>, signal_wrapped_child: bool) {
        self.shared.lock().phase = Phase::Running(Running {
            pgid,
            signal_wrapped_child,
            terminating: false,
            deadline: timeout.map(|timeout| Instant::now() + timeout),
        });
        let watcher = Arc::clone(&self.shared);
        thread::spawn(move || watcher.watch());
    }

    pub fn kill_group(&self) {
        if let Phase::Running(running) = &self.shared.lock().phase {
            kill_group(running.pgid, libc::SIGKILL);
        }
    }

    pub fn exited(&self) {
        let mut state = self.shared.lock();
        if let Phase::Running(running) = &state.phase {
            kill_group(running.pgid, libc::SIGKILL);
        }
        state.phase = Phase::Finishing;
        self.shared.changed.notify_all();
    }

    pub fn finishing(&self) {
        self.shared.lock().phase = Phase::Finishing;
        self.shared.changed.notify_all();
    }

    pub fn first_signal(&self) -> Option<Signal> {
        self.shared.lock().first_signal
    }

    pub fn timed_out(&self) -> bool {
        self.shared.lock().timed_out
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn receive(&self, signal: Signal) {
        let mut state = self.lock();
        let first = *state.first_signal.get_or_insert(signal);
        match &mut state.phase {
            Phase::Starting | Phase::Finishing => {}
            Phase::Preparing(preparation) => std::process::exit(preparation.abort(first).into()),
            Phase::Doctoring(doctoring) => std::process::exit(doctoring.abort(first).into()),
            Phase::Running(running) => {
                running.terminate(signal.number());
                self.changed.notify_all();
            }
        }
    }

    fn watch(&self) {
        let mut state = self.lock();
        loop {
            let Phase::Running(running) = &mut state.phase else {
                return;
            };
            let Some(deadline) = running.deadline else {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                continue;
            };
            let now = Instant::now();
            if now < deadline {
                state = self
                    .changed
                    .wait_timeout(state, deadline - now)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
                continue;
            }
            if running.terminating {
                kill_group(running.pgid, libc::SIGKILL);
                running.deadline = None;
            } else {
                running.terminate(SIGTERM);
                state.timed_out = true;
            }
        }
    }
}

impl Preparation {
    fn abort(&self, signal: Signal) -> u8 {
        if let Some(pid) = self.check_pid {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let status = EndStatus::Interrupted(signal);
        let end = Event::now(Body::End(End {
            status,
            exit_code: None,
            detail: String::new(),
            duration_ms: self.started.elapsed().as_millis() as u64,
            usage: Usage::default(),
            result: None,
        }));
        if let Ok(mut stdout) = self.stdout.lock() {
            Output::new(self.format, SandboxMode::On, "").write(&mut *stdout, &end, &|_| None);
        }
        if !self.keep_tempdir {
            for tempdir in &self.tempdirs {
                let _ = std::fs::remove_dir_all(tempdir);
            }
        }
        status.exit_code()
    }
}

fn kill_group(pgid: i32, signal: i32) {
    unsafe {
        libc::killpg(pgid, signal);
    }
}

fn kill_process(pid: i32, signal: i32) {
    unsafe {
        libc::kill(pid, signal);
    }
}

pub fn wrapped_child(proc_root: &Path, parent: i32) -> Option<i32> {
    let listed = std::fs::read_to_string(
        proc_root
            .join(parent.to_string())
            .join("task")
            .join(parent.to_string())
            .join("children"),
    )
    .ok()
    .and_then(|text| text.split_whitespace().next()?.parse().ok());
    listed.or_else(|| scan_for_child(proc_root, parent))
}

fn scan_for_child(proc_root: &Path, parent: i32) -> Option<i32> {
    let mut pids: Vec<i32> = std::fs::read_dir(proc_root)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    pids.sort_unstable();
    pids.into_iter()
        .find(|pid| parent_of(proc_root, *pid) == Some(parent))
}

fn parent_of(proc_root: &Path, pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let (_, after_name) = stat.rsplit_once(')')?;
    after_name.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_proc(root: &Path, pid: i32, parent: i32, children: Option<&str>) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(dir.join("task").join(pid.to_string())).unwrap();
        std::fs::write(
            dir.join("stat"),
            format!("{pid} (my (odd) name) S {parent} {pid} {pid} 0 -1 4194560 100\n"),
        )
        .unwrap();
        if let Some(children) = children {
            std::fs::write(
                dir.join("task").join(pid.to_string()).join("children"),
                children,
            )
            .unwrap();
        }
    }

    #[test]
    fn children_file_gives_the_first_pid() {
        let root = tempfile::tempdir().unwrap();
        write_proc(root.path(), 100, 1, Some("205 206 "));
        write_proc(root.path(), 205, 100, Some(""));
        write_proc(root.path(), 206, 100, None);
        assert_eq!(wrapped_child(root.path(), 100), Some(205));
    }

    #[test]
    fn empty_or_missing_children_file_falls_back_to_scanning() {
        let root = tempfile::tempdir().unwrap();
        write_proc(root.path(), 100, 1, Some("\n"));
        write_proc(root.path(), 99, 1, None);
        write_proc(root.path(), 300, 100, None);
        write_proc(root.path(), 301, 100, None);
        std::fs::create_dir(root.path().join("self")).unwrap();
        assert_eq!(wrapped_child(root.path(), 100), Some(300));
        write_proc(root.path(), 400, 1, None);
        write_proc(root.path(), 500, 400, None);
        assert_eq!(wrapped_child(root.path(), 400), Some(500));
    }

    #[test]
    fn no_child_gives_none() {
        let root = tempfile::tempdir().unwrap();
        write_proc(root.path(), 100, 1, Some(""));
        write_proc(root.path(), 101, 1, None);
        assert_eq!(wrapped_child(root.path(), 100), None);
        assert_eq!(wrapped_child(root.path(), 7), None);
        assert_eq!(wrapped_child(Path::new("/nonexistent/proc"), 1), None);
    }
}
