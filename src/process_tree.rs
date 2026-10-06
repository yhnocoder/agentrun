use std::collections::BTreeSet;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

pub const SNAPSHOT_PERIOD: Duration = Duration::from_secs(1);
#[cfg(target_os = "linux")]
pub const RECORDS_DESCENDANTS: bool = false;
#[cfg(not(target_os = "linux"))]
pub const RECORDS_DESCENDANTS: bool = true;
const REAP_WAIT: Duration = Duration::from_millis(200);
const REAP_POLL: Duration = Duration::from_millis(5);
const KILL_PASSES: usize = 3;
pub const PROC_ROOT: &str = "/proc";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Process {
    pub pid: i32,
    pub started: String,
}

struct Entry {
    pid: i32,
    parent: i32,
    started: String,
}

pub struct ProcessTable {
    entries: Vec<Entry>,
}

impl ProcessTable {
    #[cfg(target_os = "linux")]
    pub fn snapshot() -> ProcessTable {
        ProcessTable::read_proc(Path::new(PROC_ROOT))
    }

    #[cfg(target_os = "macos")]
    pub fn snapshot() -> ProcessTable {
        let mut entries: Vec<Entry> = list_pids().into_iter().filter_map(bsd_info).collect();
        entries.sort_unstable_by_key(|entry| entry.pid);
        ProcessTable { entries }
    }

    pub fn read_proc(proc_root: &Path) -> ProcessTable {
        let mut entries: Vec<Entry> = std::fs::read_dir(proc_root)
            .map(|dir| {
                dir.flatten()
                    .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                    .filter_map(|pid| read_stat(proc_root, pid))
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_unstable_by_key(|entry| entry.pid);
        ProcessTable { entries }
    }

    pub fn children_of(&self, parent: i32) -> impl Iterator<Item = Process> + '_ {
        self.entries
            .iter()
            .filter(move |entry| entry.parent == parent)
            .map(Entry::process)
    }

    pub fn descendants(&self, roots: &[i32]) -> Vec<Process> {
        let mut found = BTreeSet::new();
        let mut pending: Vec<i32> = roots.to_vec();
        while let Some(parent) = pending.pop() {
            for child in self.children_of(parent) {
                if !roots.contains(&child.pid) && found.insert(child.clone()) {
                    pending.push(child.pid);
                }
            }
        }
        found.into_iter().collect()
    }

    pub fn still_running(&self, process: &Process) -> bool {
        self.entries
            .binary_search_by_key(&process.pid, |entry| entry.pid)
            .is_ok_and(|index| self.entries[index].started == process.started)
    }
}

impl Entry {
    fn process(&self) -> Process {
        Process {
            pid: self.pid,
            started: self.started.clone(),
        }
    }
}

#[cfg(target_os = "linux")]
pub fn claim_orphans() {
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1);
    }
}

#[cfg(target_os = "linux")]
fn claims_orphans() -> bool {
    let mut claims: libc::c_int = 0;
    unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut claims) == 0 && claims != 0 }
}

#[cfg(not(target_os = "linux"))]
pub fn claim_orphans() {}

#[cfg(target_os = "macos")]
fn list_pids() -> Vec<libc::pid_t> {
    let needed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut pids = vec![0 as libc::pid_t; needed.max(0) as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(filled.clamp(0, pids.len() as libc::c_int) as usize);
    pids.retain(|pid| *pid > 0);
    pids
}

#[cfg(target_os = "macos")]
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
        started: format!("{}.{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec),
    })
}

#[cfg(not(target_os = "linux"))]
fn claims_orphans() -> bool {
    false
}

pub fn record_descendants(main: i32) -> Vec<Process> {
    ProcessTable::snapshot().descendants(&[main])
}

pub fn kill_tree(parents: &[i32], recorded: &[Process]) {
    let own = std::process::id() as i32;
    for _ in 0..KILL_PASSES {
        let table = ProcessTable::snapshot();
        let mut targets: BTreeSet<Process> = recorded
            .iter()
            .filter(|process| table.still_running(process))
            .cloned()
            .collect();
        let mut roots = parents.to_vec();
        if claims_orphans() {
            roots.push(own);
        }
        roots.extend(targets.iter().map(|process| process.pid));
        targets.extend(table.descendants(&roots));
        targets.retain(|process| process.pid != own && !parents.contains(&process.pid));
        let killed: Vec<i32> = targets
            .into_iter()
            .map(|process| process.pid)
            .filter(|pid| unsafe { libc::kill(*pid, libc::SIGKILL) } == 0)
            .collect();
        if killed.is_empty() {
            return;
        }
        reap(&killed);
    }
}

fn reap(pids: &[i32]) {
    let deadline = Instant::now() + REAP_WAIT;
    let mut remaining: Vec<i32> = pids.to_vec();
    loop {
        remaining
            .retain(|pid| unsafe { libc::waitpid(*pid, std::ptr::null_mut(), libc::WNOHANG) } == 0);
        if remaining.is_empty() || Instant::now() >= deadline {
            return;
        }
        thread::sleep(REAP_POLL);
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
    listed.or_else(|| {
        ProcessTable::read_proc(proc_root)
            .children_of(parent)
            .next()
            .map(|child| child.pid)
    })
}

fn read_stat(proc_root: &Path, pid: i32) -> Option<Entry> {
    let stat = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let (_, after_name) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = after_name.split_whitespace().collect();
    let state = *fields.first()?;
    if state == "Z" || state == "X" {
        return None;
    }
    Some(Entry {
        pid,
        parent: fields.get(1)?.parse().ok()?,
        started: fields.get(19)?.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_proc(root: &Path, pid: i32, parent: i32, children: Option<&str>) {
        write_proc_state(root, pid, parent, "S", 100, children);
    }

    fn write_proc_state(
        root: &Path,
        pid: i32,
        parent: i32,
        state: &str,
        started: u64,
        children: Option<&str>,
    ) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(dir.join("task").join(pid.to_string())).unwrap();
        std::fs::write(
            dir.join("stat"),
            format!(
                "{pid} (my (odd) name) {state} {parent} {pid} {pid} 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 {started} 1000 100\n"
            ),
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

    fn pids(processes: &[Process]) -> Vec<i32> {
        processes.iter().map(|process| process.pid).collect()
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

    #[test]
    fn descendants_follow_the_tree_from_every_root() {
        let root = tempfile::tempdir().unwrap();
        write_proc(root.path(), 100, 1, None);
        write_proc(root.path(), 200, 100, None);
        write_proc(root.path(), 300, 200, None);
        write_proc(root.path(), 400, 1, None);
        write_proc(root.path(), 500, 400, None);
        write_proc(root.path(), 600, 1, None);
        write_proc_state(root.path(), 700, 100, "Z", 100, None);
        let table = ProcessTable::read_proc(root.path());
        assert_eq!(pids(&table.descendants(&[100])), vec![200, 300]);
        assert_eq!(pids(&table.descendants(&[100, 400])), vec![200, 300, 500]);
        assert_eq!(table.descendants(&[300]), Vec::<Process>::new());
        assert_eq!(pids(&table.descendants(&[100, 200])), vec![300]);
        assert_eq!(table.descendants(&[8]), Vec::<Process>::new());
    }

    #[test]
    fn a_recorded_process_is_recognised_by_pid_and_start_time() {
        let root = tempfile::tempdir().unwrap();
        write_proc(root.path(), 100, 1, None);
        write_proc_state(root.path(), 200, 100, "S", 5000, None);
        let recorded = ProcessTable::read_proc(root.path()).descendants(&[100]);
        assert_eq!(recorded[0].started, "5000");
        assert!(ProcessTable::read_proc(root.path()).still_running(&recorded[0]));
        write_proc_state(root.path(), 200, 1, "S", 7000, None);
        assert!(!ProcessTable::read_proc(root.path()).still_running(&recorded[0]));
        std::fs::remove_dir_all(root.path().join("200")).unwrap();
        assert!(!ProcessTable::read_proc(root.path()).still_running(&recorded[0]));
    }
}
