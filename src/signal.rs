use std::io::Write;
use std::path::PathBuf;
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
    Running(Running),
    Finishing,
}

struct Preparation {
    stdout: SharedWriter,
    format: Format,
    started: Instant,
    tempdir: Option<PathBuf>,
    keep_tempdir: bool,
    check_pid: Option<i32>,
}

struct Running {
    pgid: i32,
    terminating: bool,
    deadline: Option<Instant>,
}

impl Running {
    fn terminate(&mut self, signal: i32) {
        if self.terminating {
            kill_group(self.pgid, libc::SIGKILL);
        } else {
            kill_group(self.pgid, signal);
            self.terminating = true;
            self.deadline = Some(Instant::now() + GRACE_PERIOD);
        }
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
            tempdir: None,
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

    pub fn format(&self, format: Format) {
        if let Phase::Preparing(preparation) = &mut self.shared.lock().phase {
            preparation.format = format;
        }
    }

    pub fn checking(&self, pid: Option<i32>) {
        if let Phase::Preparing(preparation) = &mut self.shared.lock().phase {
            preparation.check_pid = pid;
        }
    }

    pub fn tempdir(&self, path: PathBuf, keep: bool) {
        if let Phase::Preparing(preparation) = &mut self.shared.lock().phase {
            preparation.tempdir = Some(path);
            preparation.keep_tempdir = keep;
        }
    }

    pub fn running(&self, pgid: i32, timeout: Option<Duration>) {
        self.shared.lock().phase = Phase::Running(Running {
            pgid,
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
            Output::new(self.format, SandboxMode::On, "").write(&mut *stdout, &end);
        }
        if let (Some(tempdir), false) = (&self.tempdir, self.keep_tempdir) {
            let _ = std::fs::remove_dir_all(tempdir);
        }
        status.exit_code()
    }
}

fn kill_group(pgid: i32, signal: i32) {
    unsafe {
        libc::killpg(pgid, signal);
    }
}
