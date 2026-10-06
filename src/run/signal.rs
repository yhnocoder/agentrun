use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator;

use crate::cli::Format;
use crate::output::{End, EndStatus, Signal, write_end};
use crate::process_tree::{
    self, ForkWatcher, PROC_ROOT, ProcessTable, RECORDS_DESCENDANTS, Recorded, SNAPSHOT_PERIOD,
    wrapped_child,
};

const GRACE_PERIOD: Duration = Duration::from_secs(5);

pub type SharedWriter = Arc<Mutex<dyn Write + Send>>;

impl Signal {
    fn from_number(number: i32) -> Option<Signal> {
        match number {
            SIGINT => Some(Signal::Interrupt),
            SIGTERM => Some(Signal::Terminate),
            _ => None,
        }
    }

    fn number(self) -> i32 {
        match self {
            Signal::Interrupt => SIGINT,
            Signal::Terminate => SIGTERM,
        }
    }
}

#[derive(Clone)]
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
    held: bool,
    pending: Vec<Signal>,
}

pub struct Hold {
    shared: Arc<Shared>,
}

impl Drop for Hold {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.held = false;
        for signal in std::mem::take(&mut state.pending) {
            self.shared.handle(&mut state, signal);
        }
    }
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
        process_tree::kill_tree(self.check_pid.as_slice(), &Recorded::default());
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
    recorded: Recorded,
}

impl Running {
    fn record(&mut self) {
        if RECORDS_DESCENDANTS {
            self.recorded.refresh(&ProcessTable::snapshot(), self.pgid);
        }
    }

    fn kill_all(&mut self) {
        self.record();
        kill_group(self.pgid, libc::SIGKILL);
    }

    fn terminate(&mut self, signal: i32) {
        if self.terminating {
            self.kill_all();
            return;
        }
        self.record();
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
    fn new() -> Signals {
        Signals {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    first_signal: None,
                    timed_out: false,
                    phase: Phase::Starting,
                    held: false,
                    pending: Vec::new(),
                }),
                changed: Condvar::new(),
            }),
        }
    }

    pub fn install() -> Signals {
        let installed = Signals::new();
        let mut signals =
            iterator::Signals::new([SIGINT, SIGTERM]).expect("SIGINT and SIGTERM can be received");
        let receiver = Arc::clone(&installed.shared);
        thread::spawn(move || {
            for number in signals.forever() {
                if let Some(signal) = Signal::from_number(number) {
                    receiver.receive(signal);
                }
            }
        });
        installed
    }

    pub fn hold(&self) -> Hold {
        self.shared.lock().held = true;
        Hold {
            shared: Arc::clone(&self.shared),
        }
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
            recorded: Recorded::default(),
        });
        let watcher = Arc::clone(&self.shared);
        thread::spawn(move || watcher.watch());
        if RECORDS_DESCENDANTS {
            let recorder = Arc::clone(&self.shared);
            thread::spawn(move || recorder.record(pgid));
        }
    }

    pub fn kill_group(&self) {
        if let Phase::Running(running) = &mut self.shared.lock().phase {
            running.kill_all();
        }
    }

    pub fn exited(&self) {
        let mut state = self.shared.lock();
        if let Phase::Running(running) = &state.phase {
            process_tree::kill_group_members(running.pgid);
            process_tree::kill_tree(&[running.pgid], &running.recorded);
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
        state.first_signal.get_or_insert(signal);
        if state.held {
            state.pending.push(signal);
            return;
        }
        self.handle(&mut state, signal);
    }

    fn handle(&self, state: &mut State, signal: Signal) {
        let first = state.first_signal.unwrap_or(signal);
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
                running.kill_all();
                running.deadline = None;
            } else {
                running.terminate(SIGTERM);
                state.timed_out = true;
            }
        }
    }

    fn record(&self, main: i32) {
        let Some(watcher) = ForkWatcher::start(main) else {
            return;
        };
        while self.running_pgid().is_some() {
            watcher.wait(SNAPSHOT_PERIOD);
            let table = ProcessTable::snapshot();
            let Phase::Running(running) = &mut self.lock().phase else {
                return;
            };
            for pid in running.recorded.refresh(&table, main) {
                watcher.track(pid);
            }
        }
    }

    fn running_pgid(&self) -> Option<i32> {
        match &self.lock().phase {
            Phase::Running(running) => Some(running.pgid),
            _ => None,
        }
    }
}

impl Preparation {
    fn abort(&self, signal: Signal) -> u8 {
        if let Some(pid) = self.check_pid {
            kill_process(pid, libc::SIGKILL);
        }
        process_tree::kill_tree(self.check_pid.as_slice(), &Recorded::default());
        let status = EndStatus::Interrupted(signal);
        if let Ok(mut stdout) = self.stdout.lock() {
            write_end(
                &mut *stdout,
                self.format,
                End::early(status, String::new(), self.started),
            );
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

#[cfg(test)]
mod tests {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command};

    use super::*;

    fn preparing() -> (Signals, Arc<Mutex<Vec<u8>>>) {
        let signals = Signals::new();
        let stdout = Arc::new(Mutex::new(Vec::new()));
        signals
            .prepare(stdout.clone(), Format::Jsonl, Instant::now())
            .unwrap();
        (signals, stdout)
    }

    fn sleeper() -> Child {
        Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn signal_during_a_hold_is_forwarded_once_the_runtime_is_running() {
        let (signals, stdout) = preparing();
        let mut child = sleeper();
        let hold = signals.hold();
        signals.shared.receive(Signal::Interrupt);
        assert!(child.try_wait().unwrap().is_none());
        signals.running(child.id() as i32, None, false);
        assert!(child.try_wait().unwrap().is_none());
        drop(hold);
        let deadline = Instant::now() + Duration::from_secs(1);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("sleep was not interrupted within 1 second");
            }
            thread::sleep(Duration::from_millis(10));
        };
        signals.finishing();
        assert_eq!(status.signal(), Some(SIGINT));
        assert_eq!(signals.first_signal(), Some(Signal::Interrupt));
        assert!(stdout.lock().unwrap().is_empty());
    }

    #[test]
    fn signal_during_a_hold_that_ends_in_finishing_is_ignored() {
        let (signals, stdout) = preparing();
        let mut child = sleeper();
        let hold = signals.hold();
        signals.shared.receive(Signal::Terminate);
        signals.finishing();
        drop(hold);
        assert!(matches!(signals.shared.lock().phase, Phase::Finishing));
        assert_eq!(signals.first_signal(), Some(Signal::Terminate));
        assert!(child.try_wait().unwrap().is_none());
        assert!(stdout.lock().unwrap().is_empty());
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
