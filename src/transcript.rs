use std::fs::DirBuilder;
use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::cli::Runtime;
use crate::runtime::dsh;

const FILE_SUFFIX: &str = ".jsonl";
const UUID_GROUPS: [usize; 5] = [8, 4, 4, 4, 12];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    pub id: String,
    pub file: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transcript {
    pub id: String,
    pub entries: Vec<PathBuf>,
    pub emptied_dir: Option<PathBuf>,
}

pub fn store(session_dir: &Path, runtime: Runtime) -> PathBuf {
    session_dir.join(runtime.name())
}

pub fn create_store(store: &Path) -> Result<(), String> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(store)
        .map_err(|error| {
            format!(
                "cannot create the session directory {}: {error}",
                store.display()
            )
        })
}

pub fn locate(store: &Path, runtime: Runtime, id: &str) -> Result<Saved, String> {
    if !is_session_id(id) {
        return Err(format!("--resume {id:?} is not a session id"));
    }
    let found = match runtime {
        Runtime::Dsh => dsh::saved_session(store, id),
        Runtime::ClaudeCode | Runtime::Codex | Runtime::Pi => find_file(store, id),
    };
    found
        .map(|file| Saved {
            id: id.to_string(),
            file,
        })
        .ok_or_else(|| {
            format!(
                "no saved {} session {id} in {}",
                runtime.name(),
                store.display()
            )
        })
}

fn is_session_id(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.len() == UUID_GROUPS.len()
        && groups.iter().zip(UUID_GROUPS).all(|(group, length)| {
            group.len() == length && group.chars().all(|c| c.is_ascii_hexdigit())
        })
}

pub fn find_file(dir: &Path, id: &str) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| names(name, id))
        })
        .map(|entry| entry.path())
        .collect();
    found.sort();
    found.into_iter().next()
}

pub fn find_file_within(dir: &Path, id: &str) -> Option<PathBuf> {
    let mut found = Vec::new();
    collect_files(dir, id, &mut found);
    found.into_iter().max_by_key(|file| {
        std::fs::metadata(file)
            .and_then(|metadata| metadata.modified())
            .ok()
    })
}

fn collect_files(dir: &Path, id: &str, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_files(&entry.path(), id, found),
            Ok(kind)
                if kind.is_file()
                    && entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| names(name, id)) =>
            {
                found.push(entry.path())
            }
            _ => {}
        }
    }
}

fn names(name: &str, id: &str) -> bool {
    name.strip_suffix(FILE_SUFFIX)
        .and_then(|stem| stem.strip_suffix(id))
        .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with(['_', '-']))
}

pub fn copy_into(file: &Path, dir: &Path) -> Result<PathBuf, String> {
    let name = file
        .file_name()
        .ok_or_else(|| format!("{} has no file name", file.display()))?;
    let target = dir.join(name);
    create_store(dir)
        .and_then(|()| {
            std::fs::copy(file, &target)
                .map(|_| ())
                .map_err(|error| format!("cannot copy {}: {error}", file.display()))
        })
        .map(|()| target)
}

pub fn copy_dir_into(source: &Path, dir: &Path) -> Result<PathBuf, String> {
    let name = source
        .file_name()
        .ok_or_else(|| format!("{} has no file name", source.display()))?;
    let target = dir.join(name);
    create_store(dir)
        .and_then(|()| {
            copy_tree(source, &target)
                .map_err(|error| format!("cannot copy {}: {error}", source.display()))
        })
        .map(|()| target)
}

pub fn save(transcript: &Transcript, store: &Path) -> Result<(), String> {
    for entry in &transcript.entries {
        let Some(name) = entry.file_name() else {
            continue;
        };
        let target = store.join(name);
        if *entry == target {
            continue;
        }
        move_entry(entry, &target).map_err(|error| {
            format!(
                "cannot move {} to {}: {error}",
                entry.display(),
                store.display()
            )
        })?;
    }
    if let Some(dir) = &transcript.emptied_dir {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

fn move_entry(source: &Path, target: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(target).is_ok_and(|metadata| metadata.is_dir()) {
        std::fs::remove_dir_all(target)?;
    }
    match std::fs::rename(source, target) {
        Err(error) if error.kind() == ErrorKind::CrossesDevices => {
            copy_tree(source, target)?;
            remove_tree(source)
        }
        result => result,
    }
}

fn copy_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    if !std::fs::symlink_metadata(source)?.is_dir() {
        return std::fs::copy(source, target).map(|_| ());
    }
    DirBuilder::new().mode(0o700).create(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        copy_tree(&entry.path(), &target.join(entry.file_name()))?;
    }
    Ok(())
}

fn remove_tree(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{}\n").unwrap();
    }

    #[test]
    fn file_names_end_with_the_id_after_a_separator() {
        assert!(names(&format!("{ID}.jsonl"), ID));
        assert!(names(&format!("2026-10-19T03-02-50-431Z_{ID}.jsonl"), ID));
        assert!(names(
            &format!("rollout-2026-10-19T03-04-46-{ID}.jsonl"),
            ID
        ));
        assert!(!names(&format!("x{ID}.jsonl"), ID));
        assert!(!names(&format!("{ID}.json"), ID));
        assert!(!names(&format!("{ID}-2.jsonl"), ID));
    }

    #[test]
    fn locate_finds_a_direct_file_and_rejects_others() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(format!("rollout-1-{ID}.jsonl"));
        touch(&file);
        std::fs::create_dir(root.path().join(format!("{ID}.jsonl.d"))).unwrap();
        assert_eq!(
            locate(root.path(), Runtime::Codex, ID),
            Ok(Saved {
                id: ID.to_string(),
                file
            })
        );
        let other = "0199a1b2-c3d4-7e5f-8a9b-000000000000";
        assert_eq!(
            locate(root.path(), Runtime::Pi, other),
            Err(format!(
                "no saved pi session {other} in {}",
                root.path().display()
            ))
        );
        for partial in [
            "0c1d2e3f4a5b",
            "8a9b-0c1d2e3f4a5b",
            "0199a1b2",
            "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5g",
        ] {
            assert_eq!(
                locate(root.path(), Runtime::Codex, partial),
                Err(format!("--resume {partial:?} is not a session id"))
            );
        }
        assert_eq!(
            locate(root.path(), Runtime::Pi, "../x"),
            Err("--resume \"../x\" is not a session id".to_string())
        );
        assert_eq!(
            locate(root.path(), Runtime::Pi, ""),
            Err("--resume \"\" is not a session id".to_string())
        );
        assert!(locate(&root.path().join("missing"), Runtime::Pi, ID).is_err());
    }

    #[test]
    fn find_file_within_searches_subdirectories() {
        let root = tempfile::tempdir().unwrap();
        let file = root
            .path()
            .join(format!("sessions/2026/10/19/rollout-1-{ID}.jsonl"));
        touch(&file);
        assert_eq!(find_file_within(root.path(), ID), Some(file.clone()));
        assert_eq!(find_file(root.path(), ID), None);
        let older = root.path().join(format!("rollout-0-{ID}.jsonl"));
        touch(&older);
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&older)
            .unwrap()
            .set_modified(past)
            .unwrap();
        assert_eq!(find_file_within(root.path(), ID), Some(file));
    }

    #[test]
    fn save_moves_files_and_directories_and_replaces_older_copies() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        create_store(&store).unwrap();
        let file = root.path().join(format!("work/{ID}.jsonl"));
        let subagent = root
            .path()
            .join(format!("work/{ID}/subagents/agent-1.jsonl"));
        touch(&file);
        touch(&subagent);
        touch(&store.join(format!("{ID}/subagents/old.jsonl")));
        std::fs::write(store.join(format!("{ID}.jsonl")), "old\n").unwrap();
        let in_place = store.join("kept.jsonl");
        touch(&in_place);
        let transcript = Transcript {
            id: ID.to_string(),
            entries: vec![
                file.clone(),
                root.path().join(format!("work/{ID}")),
                in_place.clone(),
            ],
            emptied_dir: Some(root.path().join("work")),
        };
        save(&transcript, &store).unwrap();
        assert_eq!(
            std::fs::read_to_string(store.join(format!("{ID}.jsonl"))).unwrap(),
            "{}\n"
        );
        assert!(
            store
                .join(format!("{ID}/subagents/agent-1.jsonl"))
                .is_file()
        );
        assert!(!store.join(format!("{ID}/subagents/old.jsonl")).exists());
        assert!(!root.path().join("work").exists());
        assert!(in_place.is_file());
    }

    #[test]
    fn copy_tree_and_remove_tree_handle_files_and_directories() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("a");
        touch(&source.join("b/c.jsonl"));
        copy_tree(&source, &root.path().join("copy")).unwrap();
        assert!(root.path().join("copy/b/c.jsonl").is_file());
        remove_tree(&source).unwrap();
        assert!(!source.exists());
        let file = root.path().join("f.jsonl");
        touch(&file);
        copy_tree(&file, &root.path().join("g.jsonl")).unwrap();
        remove_tree(&file).unwrap();
        assert!(!file.exists() && root.path().join("g.jsonl").is_file());
    }

    #[test]
    fn copy_into_creates_the_directory_and_keeps_the_name() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(format!("store/x_{ID}.jsonl"));
        touch(&file);
        let target = copy_into(&file, &root.path().join("stage/sessions")).unwrap();
        assert_eq!(
            target,
            root.path().join(format!("stage/sessions/x_{ID}.jsonl"))
        );
        assert!(file.is_file() && target.is_file());
    }
}
