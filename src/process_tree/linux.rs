use std::path::Path;
use std::thread;
use std::time::Duration;

use super::{PROC_ROOT, ProcessTable};

pub const RECORDS_DESCENDANTS: bool = false;

pub fn claim_orphans() {
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1);
    }
}

pub(super) fn claims_orphans() -> bool {
    let mut claims: libc::c_int = 0;
    unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut claims) == 0 && claims != 0 }
}

pub(super) fn snapshot() -> ProcessTable {
    ProcessTable::read_proc(Path::new(PROC_ROOT))
}

pub struct ForkWatcher;

impl ForkWatcher {
    pub fn start(_main: i32) -> Option<ForkWatcher> {
        None
    }

    pub fn track(&self, _pid: i32) {}

    pub fn wait(&self, timeout: Duration) {
        thread::sleep(timeout);
    }
}
