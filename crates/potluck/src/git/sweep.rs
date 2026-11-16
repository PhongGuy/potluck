//! Cleanup of the debris a SIGKILLed receive-pack leaves in a bare repository.

use std::io::ErrorKind;
use std::path::Path;

/// Removes what a SIGKILLed receive-pack leaves in the bare repository `repo`: quarantine
/// directories, ref and packed-refs locks, `HEAD.lock` and receive-pack's own pack keeps.
/// Blocking. Returns how many entries it removed; a failure is logged and skipped, and a
/// missing repository removes nothing. The caller guarantees no receive-pack is running in it.
// Only the tests call it until the engine's run-limit kill is wired to it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn sweep(repo: &Path) -> usize {
    let mut removed = 0;
    for name in ["HEAD.lock", "packed-refs.lock"] {
        removed += remove(&repo.join(name), false);
    }
    removed += sweep_ref_locks(&repo.join("refs"));

    // Each quarantine directory goes whole; a kill mid index-pack leaves its tmp_pack inside.
    for entry in entries(&repo.join("objects")) {
        let quarantine = entry
            .file_name()
            .to_string_lossy()
            .starts_with("tmp_objdir-incoming-");
        if quarantine {
            removed += remove(&entry.path(), true);
        }
    }

    // receive-pack stamps its own keeps; any other keep is an operator's and stays.
    for entry in entries(&repo.join("objects/pack")) {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "keep") {
            match std::fs::read(&path) {
                Ok(text) if text.starts_with(b"receive-pack ") => removed += remove(&path, false),
                Ok(_) => {}
                Err(err) => warn_unless_gone(&path, &err),
            }
        }
    }
    removed
}

/// Removes every `*.lock` file under `dir`, recursively. A refname cannot end in `.lock`, so
/// no ref is ever among them.
fn sweep_ref_locks(dir: &Path) -> usize {
    let mut removed = 0;
    for entry in entries(dir) {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => removed += sweep_ref_locks(&path),
            Ok(_) if path.extension().is_some_and(|ext| ext == "lock") => {
                removed += remove(&path, false)
            }
            Ok(_) => {}
            Err(err) => warn_unless_gone(&path, &err),
        }
    }
    removed
}

/// The entries of `dir`, skipping any that cannot be read; a missing `dir` has none.
fn entries(dir: &Path) -> Vec<std::fs::DirEntry> {
    match std::fs::read_dir(dir) {
        Ok(read) => read
            .filter_map(|entry| entry.inspect_err(|err| warn_unless_gone(dir, err)).ok())
            .collect(),
        Err(err) => {
            warn_unless_gone(dir, &err);
            Vec::new()
        }
    }
}

/// Removes the file, or with `tree` the directory and its contents; 1 when it removed it.
fn remove(path: &Path, tree: bool) -> usize {
    let result = if tree {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => 1,
        Err(err) => {
            warn_unless_gone(path, &err);
            0
        }
    }
}

/// A path that is already gone is what the sweep wants; anything else is worth a warning.
fn warn_unless_gone(path: &Path, err: &std::io::Error) {
    if err.kind() != ErrorKind::NotFound {
        tracing::warn!(path = %path.display(), error = %err, "sweep skipped an entry");
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn sweep_removes_what_a_killed_receive_pack_leaves_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("w.git");
        let debris = [
            "objects/tmp_objdir-incoming-AbC123/pack/tmp_pack_xyz",
            "objects/tmp_objdir-incoming-AbC123/78/981922613b2afb6025042ff6bd878ac1994e85",
            "refs/heads/main.lock",
            "refs/heads/feature/deep.lock",
            "HEAD.lock",
            "packed-refs.lock",
        ];
        for path in debris {
            put(&repo.join(path), "x");
        }
        put(
            &repo.join("objects/pack/pack-aaaa.keep"),
            "receive-pack 4018551 on host\n",
        );
        let kept = [
            "HEAD",
            "config",
            "packed-refs",
            "refs/heads/main",
            "refs/heads/feature/deep",
            "objects/pack/pack-aaaa.pack",
            "objects/pack/pack-bbbb.pack",
            "objects/78/981922613b2afb6025042ff6bd878ac1994e85",
        ];
        for path in kept {
            put(&repo.join(path), "x");
        }
        put(
            &repo.join("objects/pack/pack-bbbb.keep"),
            "kept by the operator\n",
        );

        // Two quarantine-tree files count once, as their directory.
        assert_eq!(sweep(&repo), 6);
        assert!(!repo.join("objects/tmp_objdir-incoming-AbC123").exists());
        for path in &debris[2..] {
            assert!(!repo.join(path).exists(), "{path} is gone");
        }
        assert!(!repo.join("objects/pack/pack-aaaa.keep").exists());
        for path in kept {
            assert!(repo.join(path).exists(), "{path} is kept");
        }
        assert!(
            repo.join("objects/pack/pack-bbbb.keep").exists(),
            "an operator keep stays"
        );
        assert_eq!(sweep(&repo), 0, "a second sweep finds nothing");
    }

    #[test]
    fn sweeping_a_missing_repository_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(sweep(&dir.path().join("gone.git")), 0);
    }
}
