use std::time::Duration;

use super::{Entry, ProcessTable};

pub const RECORDS_DESCENDANTS: bool = true;

pub fn claim_orphans() {}

pub(super) fn claims_orphans() -> bool {
    false
}

pub(super) fn snapshot() -> ProcessTable {
    let mut entries: Vec<Entry> = list_pids().into_iter().filter_map(bsd_info).collect();
    entries.sort_unstable_by_key(|entry| entry.pid);
    ProcessTable { entries }
}

pub struct ForkWatcher {
    queue: libc::c_int,
}

impl ForkWatcher {
    pub fn start(main: i32) -> Option<ForkWatcher> {
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            return None;
        }
        let watcher = ForkWatcher { queue };
        watcher.track(main);
        Some(watcher)
    }

    pub fn track(&self, pid: i32) {
        let change = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_FORK | libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        unsafe {
            libc::kevent(
                self.queue,
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            );
        }
    }

    pub fn wait(&self, timeout: Duration) {
        let mut events: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
        let timeout = libc::timespec {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        };
        unsafe {
            libc::kevent(
                self.queue,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as libc::c_int,
                &timeout,
            );
        }
    }
}

impl Drop for ForkWatcher {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.queue);
        }
    }
}

fn list_pids() -> Vec<libc::pid_t> {
    let needed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut pids = vec![0 as libc::pid_t; needed.max(0) as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(filled.clamp(0, pids.len() as libc::c_int) as usize);
    pids.retain(|pid| *pid > 0);
    pids
}

fn bsd_info(pid: libc::pid_t) -> Option<Entry> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if got != size || info.pbi_status == libc::SZOMB {
        return None;
    }
    Some(Entry {
        pid,
        parent: info.pbi_ppid as i32,
        pgid: info.pbi_pgid as i32,
        started: format!("{}.{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec),
    })
}
