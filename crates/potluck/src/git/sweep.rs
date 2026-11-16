//! Cleanup of the debris a SIGKILLed receive-pack leaves in a bare repository.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{OwnedRwLockReadGuard, RwLock};

/// The receive-packs in flight per repository. Only the last one out of a repository that a
/// killed push left dirty sweeps it, so a sweep never touches a live push's files.
#[derive(Debug)]
pub(crate) struct Writers {
    entries: Mutex<HashMap<PathBuf, Arc<Entry>>>,
    /// The sweep each entry runs; always `sweep` outside tests, which stall it.
    sweep: fn(&Path) -> Swept,
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
    sweep: fn(&Path) -> Swept,
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
        let held = Arc::clone(&entry);
        // The blocking task owns the guard, so a dropped `leave` cannot open the gate early, and
        // re-marks a deferred sweep before the gate opens, so the next writer out retries it.
        let swept = tokio::task::spawn_blocking(move || {
            let _sweeping = sweeping;
            if (held.sweep)(&held.repo).deferred {
                held.dirty.store(true, Ordering::SeqCst);
            }
        });
        if let Err(err) = swept.await {
            tracing::warn!(repo = %entry.repo.display(), error = %err, "sweep task failed");
        }
        true
    }
}

/// What one `sweep` did: how many entries it removed, and whether it left ref locks to a live
/// gc or maintenance run, so the repository still needs a sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Swept {
    pub(crate) removed: usize,
    pub(crate) deferred: bool,
}

/// Removes what a SIGKILLed receive-pack leaves in the bare repository `repo`: quarantine
/// directories, ref and packed-refs locks, `HEAD.lock` and receive-pack's own pack keeps.
/// While a detached gc or maintenance run may be live the ref locks stay and the sweep is
/// `deferred`. Blocking. A failure is logged and skipped, and a missing repository removes
/// nothing. The caller guarantees no receive-pack is running in it.
pub(crate) fn sweep(repo: &Path) -> Swept {
    let mut removed = 0;
    let deferred = housekeeping_live(repo);
    if !deferred {
        for name in ["HEAD.lock", "packed-refs.lock"] {
            removed += remove(&repo.join(name), false);
        }
        removed += sweep_ref_locks(&repo.join("refs"));
    }

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

    // receive-pack stamps its own keeps; any other keep, or one that is not a regular file, stays.
    for entry in entries(&repo.join("objects/pack")) {
        let path = entry.path();
        let regular = entry.file_type().is_ok_and(|kind| kind.is_file());
        if regular && path.extension().is_some_and(|ext| ext == "keep") {
            match std::fs::read(&path) {
                Ok(text) if text.starts_with(b"receive-pack ") => removed += remove(&path, false),
                Ok(_) => {}
                Err(err) => warn_unless_gone(&path, &err),
            }
        }
    }
    Swept { removed, deferred }
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
                removed += sweep(&path).removed;
            }
        }
    }
    removed
}

/// How long git trusts a `gc.pid`; a lock fallback below shares the same horizon.
const HOUSEKEEPING_HORIZON: Duration = Duration::from_secs(12 * 3600);

/// True while a detached `git gc` or `git maintenance run` in this pid namespace may hold
/// `repo`'s ref locks. Every positive answer names a live git process, so it ends with that run.
fn housekeeping_live(repo: &Path) -> bool {
    gc_live(&repo.join("gc.pid")) || maintenance_live(&repo.join("objects/maintenance.lock"))
}

/// git's `gc.pid` rule, narrowed to what this host can prove: a record younger than 12 hours
/// naming this host and a pid that is still a git process. Another host's record, a pid reused
/// by a non-git process, and a malformed record are stale.
fn gc_live(pidfile: &Path) -> bool {
    if !fresh(pidfile) {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(pidfile) else {
        return false;
    };
    let mut fields = text.split_whitespace();
    let (Some(pid), Some(host)) = (fields.next(), fields.next()) else {
        return false;
    };
    let here = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    host == here.trim() && pid.parse::<u32>().is_ok_and(is_git)
}

/// A maintenance run holds `objects/maintenance.lock` open for its whole run, so the lock is live
/// while some process has it open. A git process whose fds cannot be read might be that holder,
/// so then a lock younger than 12 hours counts as live too.
fn maintenance_live(lock: &Path) -> bool {
    let Ok(lock) = std::fs::canonicalize(lock) else {
        return false;
    };
    let mut unreadable_git = false;
    for proc in entries(Path::new("/proc")) {
        let Ok(pid) = proc.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        match std::fs::read_dir(proc.path().join("fd")) {
            Ok(fds) => {
                let held = fds
                    .filter_map(Result::ok)
                    .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target == lock));
                if held {
                    return true;
                }
            }
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                unreadable_git |= is_git(pid);
            }
            Err(_) => {}
        }
    }
    unreadable_git && fresh(&lock)
}

/// Whether `pid` is a live git process: its argv[0] is `git` or a dashed `git-*` program.
fn is_git(pid: u32) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or_default();
    let name = argv0.rsplit(|&b| b == b'/').next().unwrap_or_default();
    name == b"git" || name.starts_with(b"git-")
}

/// Whether `path` was modified within the 12-hour horizon; false when it cannot be read.
fn fresh(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .is_ok_and(|at| at.elapsed().map_or(true, |age| age <= HOUSEKEEPING_HORIZON))
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
        assert_eq!(sweep(&repo).removed, 6);
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
        assert_eq!(sweep(&repo).removed, 0, "a second sweep finds nothing");
    }

    #[test]
    fn sweeping_a_missing_repository_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(sweep(&dir.path().join("gone.git")).removed, 0);
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

    #[test]
    fn a_keep_that_is_not_a_regular_file_is_skipped_without_reading_it() {
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let repo = locked_repo(dir.path());
        let fifo = repo.join("objects/pack/pack-fifo.keep");
        mkfifo(&fifo);
        let swept = std::thread::spawn({
            let repo = repo.clone();
            move || sweep(&repo)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !swept.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let stalled = !swept.is_finished();
        // Unblock before any assertion, so a failing run still ends.
        unstall(&fifo, || swept.is_finished());
        assert!(!stalled, "the sweep read the FIFO keep and blocked");
        assert_eq!(swept.join().unwrap().removed, 1, "only the ref lock goes");
        assert!(fifo.exists(), "a keep that is not a regular file stays");
    }

    /// Blocks on the FIFO `<repo>/stall` until the test opens it, removes it, then sweeps.
    fn stalling_sweep(repo: &Path) -> Swept {
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

    #[test]
    fn sweeping_every_repository_does_not_follow_symlinked_teams_or_workspaces() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repos");
        let outside = dir.path().join("outside");
        put(&outside.join("team/w.git/refs/heads/main.lock"), "x");
        put(&outside.join("w.git/HEAD.lock"), "x");
        put(&root.join("core/api.git/HEAD.lock"), "x");
        symlink(outside.join("team"), root.join("linked")).unwrap();
        symlink(outside.join("w.git"), root.join("core/linked.git")).unwrap();
        assert_eq!(sweep_all(&root), 1, "only the real workspace is swept");
        assert!(
            !root.join("core/api.git/HEAD.lock").exists(),
            "the real workspace is swept"
        );
        assert!(
            outside.join("team/w.git/refs/heads/main.lock").exists(),
            "a symlinked team is not followed"
        );
        assert!(
            outside.join("w.git/HEAD.lock").exists(),
            "a symlinked workspace is not followed"
        );
    }

    /// A `sleep 30` in its own process group holding `path` open, as a detached `git maintenance
    /// run` holds its lock from outside git's group. Killed and reaped on drop, so a failing
    /// assertion never leaves it running.
    struct Holder(std::process::Child);

    impl Holder {
        fn open(path: &Path, cwd: &Path) -> Holder {
            use std::os::unix::process::CommandExt;
            let child = std::process::Command::new("sleep")
                .arg("30")
                .stdin(std::fs::File::open(path).unwrap())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .current_dir(cwd)
                .process_group(0)
                .spawn()
                .unwrap();
            Holder(child)
        }

        /// A live git process (`hash-object` waiting on its open stdin) in its own group.
        fn git(cwd: &Path) -> Holder {
            use std::os::unix::process::CommandExt;
            let child = std::process::Command::new("git")
                .args(["hash-object", "--stdin"])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .current_dir(cwd)
                .process_group(0)
                .spawn()
                .unwrap();
            Holder(child)
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Backdates `path` past git's 12-hour horizon, so only a live holder can keep it live.
    fn age(path: &Path) {
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(13 * 3600);
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(then).unwrap();
    }

    fn hostname() -> String {
        let name = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        name.trim().to_owned()
    }

    const REF_LOCKS: [&str; 3] = ["HEAD.lock", "packed-refs.lock", "refs/heads/main.lock"];

    fn ref_locked_repo(dir: &Path) -> std::path::PathBuf {
        let repo = dir.join("w.git");
        for path in REF_LOCKS {
            put(&repo.join(path), "x");
        }
        put(&repo.join("HEAD"), "ref: refs/heads/main\n");
        repo
    }

    #[tokio::test]
    async fn a_live_maintenance_run_keeps_its_ref_locks_through_a_killed_push_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ref_locked_repo(dir.path());
        put(
            &repo.join("objects/tmp_objdir-incoming-AbC123/pack/tmp_pack_xyz"),
            "x",
        );
        let lock = repo.join("objects/maintenance.lock");
        put(&lock, "");
        age(&lock);
        let holder = Holder::open(&lock, &repo);
        let writers = Writers::default();
        assert!(
            writers.enter(&repo).await.leave(true).await,
            "the killed push sweeps"
        );
        for path in REF_LOCKS {
            assert!(
                repo.join(path).exists(),
                "{path} survives the live maintenance run"
            );
        }
        assert!(
            !repo.join("objects/tmp_objdir-incoming-AbC123").exists(),
            "the quarantine still goes"
        );
        drop(holder);
        assert!(
            writers.enter(&repo).await.leave(false).await,
            "the repository stayed dirty, so the next writer out sweeps"
        );
        for path in REF_LOCKS {
            assert!(
                !repo.join(path).exists(),
                "{path} goes once the run has ended"
            );
        }
        assert!(lock.exists(), "the maintenance lock is git's, never swept");
    }

    #[test]
    fn a_gc_pid_keeps_ref_locks_only_while_it_names_a_live_git_on_this_host() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ref_locked_repo(dir.path());
        let pidfile = repo.join("gc.pid");
        let gc = Holder::git(&repo);
        put(&pidfile, &format!("{} {}", gc.pid(), hostname()));
        sweep(&repo);
        for path in REF_LOCKS {
            assert!(
                repo.join(path).exists(),
                "{path} survives a live gc on this host"
            );
        }
        drop(gc);
        sweep(&repo);
        for path in REF_LOCKS {
            assert!(
                !repo.join(path).exists(),
                "{path} goes once that gc is gone"
            );
        }
        assert!(pidfile.exists(), "gc.pid is git's, never swept");

        // After a restart the pid can name an unrelated process, and the host can be renamed.
        let reused = ref_locked_repo(&dir.path().join("reused"));
        let other = Holder::open(&reused.join("HEAD"), &reused);
        put(
            &reused.join("gc.pid"),
            &format!("{} {}", other.pid(), hostname()),
        );
        let renamed = ref_locked_repo(&dir.path().join("renamed"));
        put(&renamed.join("gc.pid"), "1 potluck-elsewhere.invalid");
        sweep(&reused);
        sweep(&renamed);
        drop(other);
        assert!(
            !reused.join("packed-refs.lock").exists(),
            "a pid reused by a process that is not git is stale"
        );
        assert!(
            !renamed.join("packed-refs.lock").exists(),
            "another host's gc.pid cannot be proven live"
        );
    }

    #[test]
    fn an_unheld_or_unreadable_housekeeping_file_does_not_keep_ref_locks() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ref_locked_repo(dir.path());
        let lock = repo.join("objects/maintenance.lock");
        put(&lock, "");
        age(&lock);
        put(&repo.join("gc.pid"), "not a pid record");
        sweep(&repo);
        for path in REF_LOCKS {
            assert!(
                !repo.join(path).exists(),
                "{path} goes: no live gc or maintenance run"
            );
        }
    }
}
