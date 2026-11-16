//! Cleanup of the debris a SIGKILLed receive-pack leaves in a bare repository.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{OwnedRwLockReadGuard, RwLock};

/// The receive-packs in flight per repository. Only the last one out of a repository that a
/// killed push left dirty sweeps it, so a sweep never touches a live push's files.
#[derive(Debug)]
pub(crate) struct Writers {
    entries: Mutex<HashMap<PathBuf, Arc<Entry>>>,
    /// The sweep each entry runs; always `sweep` outside tests, which stall it.
    sweep: fn(&Path) -> usize,
}

impl Default for Writers {
    fn default() -> Self {
        Writers {
            entries: Mutex::default(),
            sweep,
        }
    }
}

/// One repository's gate: writers hold it shared, a sweep holds it exclusive.
#[derive(Debug)]
struct Entry {
    repo: PathBuf,
    gate: Arc<RwLock<()>>,
    dirty: AtomicBool,
    sweep: fn(&Path) -> usize,
}

/// One receive-pack inside a repository, from just before spawn until its reaper is done.
#[derive(Debug)]
pub(crate) struct Writer {
    entry: Arc<Entry>,
    guard: OwnedRwLockReadGuard<()>,
}

impl Writers {
    /// Waits only while a sweep of `repo` runs.
    pub(crate) async fn enter(&self, repo: &Path) -> Writer {
        let entry = self.entry(repo);
        let guard = Arc::clone(&entry.gate).read_owned().await;
        Writer { entry, guard }
    }

    /// The gate for `repo`, created on first use; the map lock is never held across an await.
    fn entry(&self, repo: &Path) -> Arc<Entry> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = entries.entry(repo.to_path_buf()).or_insert_with(|| {
            Arc::new(Entry {
                repo: repo.to_path_buf(),
                gate: Arc::new(RwLock::new(())),
                dirty: AtomicBool::new(false),
                sweep: self.sweep,
            })
        });
        Arc::clone(entry)
    }
}

impl Writer {
    /// Leaves the repository; `killed` marks it dirty first. Sweeps when it is dirty and no
    /// other writer is inside, and returns whether this call swept.
    pub(crate) async fn leave(self, killed: bool) -> bool {
        let Writer { entry, guard } = self;
        // Mark before releasing, so a clean writer leaving at the same moment sees the flag.
        if killed {
            entry.dirty.store(true, Ordering::SeqCst);
        }
        drop(guard);
        if !entry.dirty.load(Ordering::SeqCst) {
            return false;
        }
        // Failing to take the gate means another writer is inside and will sweep on its way out.
        let Ok(sweeping) = Arc::clone(&entry.gate).try_write_owned() else {
            return false;
        };
        if !entry.dirty.swap(false, Ordering::SeqCst) {
            return false;
        }
        let (repo, sweep) = (entry.repo.clone(), entry.sweep);
        // The blocking task owns the guard, so a dropped `leave` cannot open the gate early.
        let swept = tokio::task::spawn_blocking(move || {
            let _sweeping = sweeping;
            sweep(&repo)
        });
        if let Err(err) = swept.await {
            tracing::warn!(repo = %entry.repo.display(), error = %err, "sweep task failed");
        }
        true
    }
}

/// Removes what a SIGKILLed receive-pack leaves in the bare repository `repo`: quarantine
/// directories, ref and packed-refs locks, `HEAD.lock` and receive-pack's own pack keeps.
/// Blocking. Returns how many entries it removed; a failure is logged and skipped, and a
/// missing repository removes nothing. The caller guarantees no receive-pack is running in it.
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

/// Sweeps every workspace repository, each `<root>/<team>/<name>.git` directory, with `sweep`.
/// Blocking. Returns the total removed; a missing `root` removes nothing. The caller
/// guarantees no receive-pack is running in any of them.
pub(crate) fn sweep_all(root: &Path) -> usize {
    let is_dir = |entry: &std::fs::DirEntry| entry.file_type().is_ok_and(|kind| kind.is_dir());
    let mut removed = 0;
    for team in entries(root).iter().filter(|entry| is_dir(entry)) {
        for repo in entries(&team.path()).iter().filter(|entry| is_dir(entry)) {
            let path = repo.path();
            if path.extension().is_some_and(|ext| ext == "git") {
                removed += sweep(&path);
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
    use std::sync::Arc;

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

    fn locked_repo(dir: &Path) -> std::path::PathBuf {
        let repo = dir.join("w.git");
        put(&repo.join("refs/heads/main.lock"), "x");
        repo
    }

    #[tokio::test]
    async fn a_deferred_sweep_runs_when_the_last_writer_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let repo = locked_repo(dir.path());
        let writers = Writers::default();
        let live = writers.enter(&repo).await;
        let killed = writers.enter(&repo).await;
        assert!(!killed.leave(true).await, "another writer is inside");
        assert!(repo.join("refs/heads/main.lock").exists(), "deferred");
        assert!(live.leave(false).await, "the last one out sweeps");
        assert!(!repo.join("refs/heads/main.lock").exists());
    }

    #[tokio::test]
    async fn a_clean_repository_is_never_swept() {
        let dir = tempfile::tempdir().unwrap();
        let repo = locked_repo(dir.path());
        let writers = Writers::default();
        assert!(!writers.enter(&repo).await.leave(false).await);
        assert!(
            repo.join("refs/heads/main.lock").exists(),
            "no kill, no sweep"
        );
    }

    #[tokio::test]
    async fn a_dirty_flag_is_cleared_by_its_sweep_and_repositories_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = locked_repo(dir.path());
        let other = dir.path().join("o.git");
        put(&other.join("refs/heads/main.lock"), "x");
        let writers = Writers::default();
        let busy_elsewhere = writers.enter(&other).await;
        assert!(
            writers.enter(&repo).await.leave(true).await,
            "alone in its repository"
        );
        assert!(
            other.join("refs/heads/main.lock").exists(),
            "other repo untouched"
        );
        put(&repo.join("refs/heads/main.lock"), "x");
        assert!(
            !writers.enter(&repo).await.leave(false).await,
            "the earlier sweep cleared the flag"
        );
        assert!(
            !busy_elsewhere.leave(false).await,
            "other repo was never dirty"
        );
    }

    #[tokio::test]
    async fn entering_waits_while_a_sweep_holds_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let repo = locked_repo(dir.path());
        let writers = Writers::default();
        let entry = writers.entry(&repo);
        let sweeping = Arc::clone(&entry.gate).try_write_owned().unwrap();
        let waiting =
            tokio::time::timeout(std::time::Duration::from_millis(200), writers.enter(&repo)).await;
        assert!(waiting.is_err(), "enter waits for the sweep");
        drop(sweeping);
        writers.enter(&repo).await.leave(false).await;
    }

    /// Opens `fifo` read-write, which never blocks on Linux and wakes a reader stalled in its open,
    /// every 20 ms until `done` holds or 10 s pass, so no test can hang on a FIFO it made.
    fn unstall(fifo: &Path, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done() && std::time::Instant::now() < deadline {
            drop(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(fifo),
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn mkfifo(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(made.success(), "mkfifo made {}", path.display());
    }

    /// Blocks on the FIFO `<repo>/stall` until the test opens it, removes it, then sweeps.
    fn stalling_sweep(repo: &Path) -> usize {
        let stall = repo.join("stall");
        let _ = std::fs::read(&stall);
        let _ = std::fs::remove_file(&stall);
        sweep(repo)
    }

    #[tokio::test]
    async fn a_dropped_leave_keeps_the_repository_closed_until_its_sweep_ends() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let repo = locked_repo(dir.path());
        let stall = repo.join("stall");
        mkfifo(&stall);
        let writers = Writers {
            entries: Mutex::default(),
            sweep: stalling_sweep,
        };
        let killed = writers.enter(&repo).await;
        let leaving = tokio::time::timeout(Duration::from_millis(300), killed.leave(true)).await;
        let entered = tokio::time::timeout(Duration::from_millis(300), writers.enter(&repo)).await;
        // Unstall the sweep before any assertion: the runtime's shutdown waits for blocking tasks.
        unstall(&stall, || !stall.exists());
        assert!(leaving.is_err(), "the sweep is stalled on the FIFO");
        assert!(
            entered.is_err(),
            "enter waits while the dropped leave's sweep still runs"
        );
        let after = tokio::time::timeout(Duration::from_secs(10), writers.enter(&repo))
            .await
            .expect("the repository opens once the sweep ends");
        assert!(!after.leave(false).await, "the sweep cleared the flag");
        assert!(
            !repo.join("refs/heads/main.lock").exists(),
            "the lock is swept"
        );
    }

    #[test]
    fn sweeping_every_repository_reaches_each_workspace_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repos");
        put(&root.join("core/api.git/refs/heads/main.lock"), "x");
        put(&root.join("ops/infra.git/HEAD.lock"), "x");
        put(&root.join("core/notes/refs/heads/main.lock"), "x");
        put(&root.join("core/file.git"), "x");
        put(&root.join("stray.lock"), "x");
        assert_eq!(sweep_all(&root), 2, "one lock in each workspace");
        assert!(
            !root.join("core/api.git/refs/heads/main.lock").exists(),
            "core/api swept"
        );
        assert!(
            !root.join("ops/infra.git/HEAD.lock").exists(),
            "ops/infra swept"
        );
        assert!(
            root.join("core/notes/refs/heads/main.lock").exists(),
            "not a workspace repository"
        );
        assert!(
            root.join("core/file.git").exists(),
            "a file is not a repository"
        );
        assert!(
            root.join("stray.lock").exists(),
            "the root itself is not swept"
        );
        assert_eq!(
            sweep_all(&dir.path().join("missing")),
            0,
            "no root, nothing removed"
        );
    }
}
