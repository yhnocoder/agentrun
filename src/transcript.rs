use std::fs::DirBuilder;
use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use uuid::Uuid;
use uuid::fmt::Hyphenated;

use crate::cli::Runtime;
use crate::runtime::dsh;

const FILE_SUFFIX: &str = ".jsonl";
const STAGED_SUFFIX: &str = ".agentrun-saving";
const REPLACED_SUFFIX: &str = ".agentrun-replaced";

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

pub fn locate(session_dir: &Path, runtime: Runtime, id: &str) -> Result<Saved, String> {
    if !is_session_id(id) {
        return Err(format!("--resume {id:?} is not a session id"));
    }
    let own = store(session_dir, runtime);
    recover(&own);
    if let Some(file) = saved(&own, runtime, id) {
        return Ok(Saved {
            id: id.to_string(),
            file,
        });
    }
    let missing = format!(
        "no saved {} session {id} in {}",
        runtime.name(),
        own.display()
    );
    let saved_by = Runtime::value_variants()
        .iter()
        .copied()
        .filter(|other| *other != runtime)
        .find(|other| saved(&store(session_dir, *other), *other, id).is_some());
    Err(match saved_by {
        Some(other) => format!(
            "{missing}; it was saved by {0}, continue it with agentrun {0}",
            other.name()
        ),
        None => missing,
    })
}

fn saved(store: &Path, runtime: Runtime, id: &str) -> Option<PathBuf> {
    match runtime {
        Runtime::Dsh => dsh::saved_session(store, id),
        Runtime::ClaudeCode | Runtime::Codex | Runtime::Pi => find_file(store, id),
    }
}

fn is_session_id(id: &str) -> bool {
    id.len() == Hyphenated::LENGTH && Uuid::try_parse(id).is_ok()
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveError {
    pub detail: String,
    pub kept: Vec<PathBuf>,
}

pub fn save(transcript: &Transcript, store: &Path) -> Result<(), SaveError> {
    recover(store);
    for (index, entry) in transcript.entries.iter().enumerate() {
        let Some(name) = entry.file_name() else {
            continue;
        };
        let target = store.join(name);
        if *entry == target {
            continue;
        }
        move_entry(entry, &target).map_err(|error| SaveError {
            detail: format!(
                "cannot move {} to {}: {error}",
                entry.display(),
                store.display()
            ),
            kept: transcript.entries[index..]
                .iter()
                .filter(|entry| entry.parent() != Some(store))
                .cloned()
                .collect(),
        })?;
    }
    if let Some(dir) = &transcript.emptied_dir {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

fn recover(store: &Path) {
    let Ok(entries) = std::fs::read_dir(store) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(target_name) = name
            .to_str()
            .and_then(|name| name.strip_prefix('.'))
            .and_then(|name| name.strip_suffix(REPLACED_SUFFIX))
        else {
            continue;
        };
        let target = store.join(target_name);
        if std::fs::symlink_metadata(&target).is_ok() {
            let _ = remove_tree(&entry.path());
        } else {
            let _ = std::fs::rename(entry.path(), &target);
        }
    }
}

fn move_entry(source: &Path, target: &Path) -> std::io::Result<()> {
    let staged = beside(target, STAGED_SUFFIX);
    remove_if_present(&staged)?;
    let copied = stage(source, &staged).inspect_err(|_| {
        let _ = remove_if_present(&staged);
    })?;
    replace(&staged, target).inspect_err(|_| {
        if copied {
            let _ = remove_if_present(&staged);
        } else {
            let _ = std::fs::rename(&staged, source);
        }
    })?;
    if copied {
        remove_tree(source)?;
    }
    Ok(())
}

fn stage(source: &Path, staged: &Path) -> std::io::Result<bool> {
    match std::fs::rename(source, staged) {
        Err(error) if error.kind() == ErrorKind::CrossesDevices => {
            copy_tree(source, staged).map(|()| true)
        }
        result => result.map(|()| false),
    }
}

fn replace(staged: &Path, target: &Path) -> std::io::Result<()> {
    if !std::fs::symlink_metadata(target).is_ok_and(|metadata| metadata.is_dir()) {
        return std::fs::rename(staged, target);
    }
    let replaced = beside(target, REPLACED_SUFFIX);
    remove_if_present(&replaced)?;
    std::fs::rename(target, &replaced)?;
    if let Err(error) = std::fs::rename(staged, target) {
        let _ = std::fs::rename(&replaced, target);
        return Err(error);
    }
    let _ = remove_tree(&replaced);
    Ok(())
}

fn beside(target: &Path, suffix: &str) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(target.file_name().unwrap_or_default());
    name.push(suffix);
    target.with_file_name(name)
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match remove_tree(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn copy_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    let kind = std::fs::symlink_metadata(source)?.file_type();
    if kind.is_file() {
        return std::fs::copy(source, target).map(|_| ());
    }
    if !kind.is_dir() {
        return Ok(());
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
        let codex = store(root.path(), Runtime::Codex);
        std::fs::create_dir(&codex).unwrap();
        let file = codex.join(format!("rollout-1-{ID}.jsonl"));
        touch(&file);
        std::fs::create_dir(codex.join(format!("{ID}.jsonl.d"))).unwrap();
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
                root.path().join("pi").display()
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
    fn locate_names_the_runtime_that_saved_the_session() {
        let root = tempfile::tempdir().unwrap();
        let codex = store(root.path(), Runtime::Codex);
        std::fs::create_dir(&codex).unwrap();
        touch(&codex.join(format!("rollout-1-{ID}.jsonl")));
        assert_eq!(
            locate(root.path(), Runtime::Pi, ID),
            Err(format!(
                "no saved pi session {ID} in {}; it was saved by codex, continue it with agentrun codex",
                root.path().join("pi").display()
            ))
        );
    }

    #[test]
    fn locate_names_dsh_when_its_session_directory_matches() {
        let root = tempfile::tempdir().unwrap();
        let dsh = store(root.path(), Runtime::Dsh);
        std::fs::create_dir_all(dsh.join(format!("session-{ID}"))).unwrap();
        assert_eq!(
            locate(root.path(), Runtime::Dsh, ID).map(|saved| saved.file),
            Ok(dsh.join(format!("session-{ID}")))
        );
        assert_eq!(
            locate(root.path(), Runtime::ClaudeCode, ID),
            Err(format!(
                "no saved claude-code session {ID} in {}; it was saved by dsh, continue it with agentrun dsh",
                root.path().join("claude-code").display()
            ))
        );
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
    fn a_failed_save_keeps_the_older_copy_and_leaves_nothing_staged() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        create_store(&store).unwrap();
        let saved_dir = store.join(format!("session-{ID}"));
        std::fs::create_dir(&saved_dir).unwrap();
        std::fs::write(saved_dir.join("log.jsonl"), "old\n").unwrap();
        let saved_file = store.join(format!("{ID}.jsonl"));
        std::fs::write(&saved_file, "old\n").unwrap();
        let transcript = |name: String| Transcript {
            id: ID.to_string(),
            entries: vec![root.path().join("missing").join(name)],
            emptied_dir: None,
        };
        assert!(save(&transcript(format!("session-{ID}")), &store).is_err());
        assert!(save(&transcript(format!("{ID}.jsonl")), &store).is_err());
        assert_eq!(
            std::fs::read_to_string(saved_dir.join("log.jsonl")).unwrap(),
            "old\n"
        );
        assert_eq!(std::fs::read_to_string(&saved_file).unwrap(), "old\n");
        assert_eq!(std::fs::read_dir(&store).unwrap().count(), 2);
    }

    #[test]
    fn a_target_that_cannot_be_replaced_leaves_the_new_session_where_it_was() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        create_store(&store).unwrap();
        std::fs::write(store.join(ID), "in the way\n").unwrap();
        let file = root.path().join(format!("work/{ID}.jsonl"));
        let dir = root.path().join(format!("work/{ID}"));
        touch(&file);
        touch(&dir.join("subagents/agent-1.jsonl"));
        let transcript = Transcript {
            id: ID.to_string(),
            entries: vec![file.clone(), dir.clone()],
            emptied_dir: None,
        };
        let error = save(&transcript, &store).unwrap_err();
        assert_eq!(error.kept, vec![dir.clone()]);
        assert!(dir.join("subagents/agent-1.jsonl").is_file());
        assert!(!file.exists() && store.join(format!("{ID}.jsonl")).is_file());
        assert_eq!(
            std::fs::read_to_string(store.join(ID)).unwrap(),
            "in the way\n"
        );
        assert_eq!(std::fs::read_dir(&store).unwrap().count(), 2);
    }

    #[test]
    fn an_older_copy_left_aside_by_an_interrupted_save_is_found_again() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path(), Runtime::Dsh);
        let name = format!("session-{ID}");
        touch(&store.join(format!(".{name}{REPLACED_SUFFIX}/log.jsonl")));
        assert_eq!(
            locate(root.path(), Runtime::Dsh, ID).unwrap().file,
            store.join(&name)
        );
        assert!(store.join(format!("{name}/log.jsonl")).is_file());
        assert_eq!(std::fs::read_dir(&store).unwrap().count(), 1);
    }

    #[test]
    fn save_clears_what_an_interrupted_save_left_behind() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        let name = format!("session-{ID}");
        touch(&store.join(format!(".{name}{STAGED_SUFFIX}/partial.jsonl")));
        touch(&store.join(format!(".{name}{REPLACED_SUFFIX}/old.jsonl")));
        touch(&store.join(format!("{name}/old.jsonl")));
        let source = root.path().join("work").join(&name);
        touch(&source.join("new.jsonl"));
        let transcript = Transcript {
            id: ID.to_string(),
            entries: vec![source.clone()],
            emptied_dir: None,
        };
        save(&transcript, &store).unwrap();
        let names: Vec<_> = std::fs::read_dir(&store)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from(&name)]);
        assert!(store.join(format!("{name}/new.jsonl")).is_file());
        assert!(!store.join(format!("{name}/old.jsonl")).exists());
        assert!(!source.exists());
    }

    #[test]
    fn save_across_file_systems_replaces_the_older_copy() {
        use std::os::unix::fs::MetadataExt;
        let shm = Path::new("/dev/shm");
        let Ok(work) = tempfile::tempdir_in(shm) else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let device = |path: &Path| std::fs::metadata(path).unwrap().dev();
        if device(work.path()) == device(root.path()) {
            return;
        }
        let store = root.path().join("store");
        let name = format!("session-{ID}");
        touch(&store.join(format!("{name}/old.jsonl")));
        std::fs::write(store.join(format!("{ID}.jsonl")), "old\n").unwrap();
        let dir = work.path().join(&name);
        touch(&dir.join("new.jsonl"));
        let file = work.path().join(format!("{ID}.jsonl"));
        std::fs::write(&file, "new\n").unwrap();
        let transcript = Transcript {
            id: ID.to_string(),
            entries: vec![dir.clone(), file.clone()],
            emptied_dir: None,
        };
        save(&transcript, &store).unwrap();
        assert!(store.join(format!("{name}/new.jsonl")).is_file());
        assert!(!store.join(format!("{name}/old.jsonl")).exists());
        assert_eq!(
            std::fs::read_to_string(store.join(format!("{ID}.jsonl"))).unwrap(),
            "new\n"
        );
        assert!(!dir.exists() && !file.exists());
        assert_eq!(std::fs::read_dir(&store).unwrap().count(), 2);
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
    fn copy_tree_skips_symbolic_links() {
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join("secret");
        touch(&secret.join("key"));
        let source = root.path().join("session");
        touch(&source.join("log.jsonl"));
        std::os::unix::fs::symlink(secret.join("key"), source.join("file-link")).unwrap();
        std::os::unix::fs::symlink(&secret, source.join("dir-link")).unwrap();
        std::os::unix::fs::symlink(root.path().join("missing"), source.join("dangling")).unwrap();
        let copy = root.path().join("copy");
        copy_tree(&source, &copy).unwrap();
        assert!(copy.join("log.jsonl").is_file());
        for name in ["file-link", "dir-link", "dangling"] {
            assert!(
                std::fs::symlink_metadata(copy.join(name)).is_err(),
                "{name}"
            );
        }
        let linked = root.path().join("linked");
        std::os::unix::fs::symlink(&source, &linked).unwrap();
        copy_tree(&linked, &root.path().join("copy2")).unwrap();
        assert!(!root.path().join("copy2").exists());
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
