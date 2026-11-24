//! Online backup: a consistent copy of a running server's data directory, streamed as a tar.
//!
//! Each workspace repository is copied by a local `git fetch` into a staging directory, the
//! database is snapshotted with SQLite's backup API, and the staged tree is written as a tar.

mod archive;
mod copy;
mod snapshot;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The oldest schema a backup can read: version 3 adds the audit log.
const MIN_SCHEMA: u32 = 3;

/// How far a backup has got, told to the caller between steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Phase {
    /// Every workspace repository is staged.
    ReposCopied,
    /// The database snapshot is staged.
    DbSnapshotted,
}

/// How a backup runs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BackupOptions {
    /// Free space kept beyond the staged copy's size; the default is the request spool bound
    /// at serve's defaults, `2 * cores * 100 MiB`.
    pub reserve_bytes: u64,
}

impl Default for BackupOptions {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let slots = u64::try_from((2 * cores).max(2)).unwrap_or(u64::MAX);
        Self {
            reserve_bytes: slots.saturating_mul(100 * 1024 * 1024),
        }
    }
}

impl BackupOptions {
    /// The same options with `reserve_bytes` free space kept beyond the staged copy.
    #[must_use]
    pub fn with_reserve_bytes(self, reserve_bytes: u64) -> Self {
        Self { reserve_bytes }
    }
}

/// What a finished backup holds and what it left out or changed.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Written {
    /// Workspaces in the archive.
    pub workspaces: usize,
    /// Workspace rows dropped from the staged database.
    pub dropped_rows: usize,
    /// Staged repositories dropped for want of a row.
    pub dropped_repos: usize,
    /// Armed overrides removed from the staged database.
    pub disarmed: usize,
    /// Workspaces left out because they changed during the backup, sorted, one entry each.
    pub left_out: Vec<LeftOut>,
}

/// A workspace the backup left out because it changed while the backup ran.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LeftOut {
    /// The team's slug.
    pub team: String,
    /// The workspace's slug.
    pub workspace: String,
    /// Why it was left out.
    pub reason: LeftOutReason,
}

/// Why a workspace was left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LeftOutReason {
    /// It was deleted, or deleted and created again, while the backup ran.
    Deleted,
    /// It was created after the repositories were copied.
    Created,
}

impl std::fmt::Display for LeftOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let why = match self.reason {
            LeftOutReason::Deleted => "deleted",
            LeftOutReason::Created => "created",
        };
        write!(
            f,
            "left out: {}/{} ({why} during the backup)",
            self.team, self.workspace
        )
    }
}

/// Why a backup was refused or failed. A failure after the tar started means the output is
/// incomplete and must be discarded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackupError {
    /// The output is a terminal.
    #[error("refusing to write a backup to a terminal; redirect stdout to a file")]
    Terminal,
    /// The data directory has no database.
    #[error("no server.db at {path}")]
    NoDatabase {
        /// The data directory.
        path: PathBuf,
    },
    /// The database predates the audit log.
    #[error("server.db is at schema version {version}; a backup needs 3 or later")]
    OldSchema {
        /// The database's `user_version`.
        version: u32,
    },
    /// This process's user does not own the database.
    #[error("run the backup as the user that owns {path}")]
    NotOwner {
        /// The database file.
        path: PathBuf,
    },
    /// Another backup holds the backup lock.
    #[error("another backup is running on this data directory")]
    InProgress,
    /// The volume lacks room for the staged copy plus the reserve.
    #[error("not enough free space: the backup needs {needed} bytes and {free} are free")]
    NoSpace {
        /// Bytes the staged copy and the reserve need.
        needed: u64,
        /// Bytes free on the volume.
        free: u64,
    },
    /// Copying a workspace's repository failed while its row still exists.
    #[error("copying workspace {workspace} failed at git {step}: {stderr}")]
    Git {
        /// The workspace as `team/ws`.
        workspace: String,
        /// The git step that failed.
        step: &'static str,
        /// Git's error output, trimmed.
        stderr: String,
    },
    /// Reading or snapshotting the database failed; the text is SQLite's message.
    #[error("reading the database failed: {0}")]
    Database(String),
    /// A filesystem step under the data directory failed.
    #[error("cannot read or write {path}")]
    Io {
        /// The path the step touched.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// Writing the archive to the output failed.
    #[error("cannot write the backup to the output")]
    Output(#[source] io::Error),
}

impl From<snapshot::SnapshotError> for BackupError {
    fn from(err: snapshot::SnapshotError) -> Self {
        BackupError::Database(err.0)
    }
}

/// Writes a tar of `data` (`server.db`, then `repos/`) to `out`, consistent while the server
/// keeps running. Refuses before copying anything when there is no database, this user does
/// not own it, its schema is older than 3, another backup holds `<data>/tmp/backup.lock`, or
/// the volume lacks room for the staged copy plus `options.reserve_bytes`. `on_phase` is told
/// when the repositories and then the database are staged. Staging under `<data>/tmp/backup/`
/// is removed on every return once the lock is held.
pub fn write_backup(
    data: &Path,
    git: &Path,
    options: &BackupOptions,
    out: &mut dyn Write,
    on_phase: &mut dyn FnMut(Phase),
) -> Result<Written, BackupError> {
    let db = data.join("server.db");
    refuse_early(&db)?;
    let _lock = take_lock(&data.join("tmp"))?;
    check_space(data, &db, options.reserve_bytes)?;

    let staging = Staging::clear(data.join("tmp").join("backup"))?;
    let (mark, mut left_out) = copy_repos(data, git, &db, &staging.0)?;
    on_phase(Phase::ReposCopied);
    let staged_db = staging.0.join("server.db");
    snapshot::snapshot(&db, &staged_db)?;
    on_phase(Phase::DbSnapshotted);

    let repos = staging.0.join("repos");
    let states = staged_states(git, &repos)?;
    let reconciled = snapshot::reconcile(&staged_db, &mark, &|team, ws| {
        states
            .get(&(team.to_owned(), ws.to_owned()))
            .copied()
            .unwrap_or(snapshot::StagedRepo::Missing)
    })?;
    // A staged copy whose row the snapshot dropped or never had belongs to a workspace deleted
    // during the backup; a deletion wins over a creation when both name the same workspace.
    let kept: BTreeSet<&(String, String)> = reconciled.kept.iter().collect();
    let mut dropped_repos = 0;
    for key in states.into_keys() {
        if kept.contains(&key) {
            continue;
        }
        let (team, ws) = key;
        remove_dir(&repos.join(&team).join(format!("{ws}.git")))?;
        dropped_repos += 1;
        left_out.insert((team, ws), LeftOutReason::Deleted);
    }
    for key in reconciled.created {
        left_out.entry(key).or_insert(LeftOutReason::Created);
    }
    remove_empty_teams(&repos)?;

    // The archive writer reads staging and writes `out`; only a failure of `out` itself is an
    // output error, anything else is a read of the staged tree.
    let mut sink = Sink {
        inner: out,
        failed: false,
    };
    archive::write_tar(&staging.0, &mut sink).map_err(|source| {
        if sink.failed {
            BackupError::Output(source)
        } else {
            BackupError::Io {
                path: staging.0.clone(),
                source,
            }
        }
    })?;
    Ok(Written {
        workspaces: reconciled.kept.len(),
        dropped_rows: reconciled.dropped_rows,
        dropped_repos,
        disarmed: reconciled.disarmed,
        left_out: left_out
            .into_iter()
            .map(|((team, workspace), reason)| LeftOut {
                team,
                workspace,
                reason,
            })
            .collect(),
    })
}

/// The refusals that need no lock: no database, another owner, an old schema.
fn refuse_early(db: &Path) -> Result<(), BackupError> {
    refuse_early_as(db, rustix::process::getuid().as_raw())
}

/// `refuse_early` for a process running as `uid`.
fn refuse_early_as(db: &Path, uid: u32) -> Result<(), BackupError> {
    let meta = match fs::metadata(db) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(BackupError::NoDatabase {
                path: db.parent().unwrap_or(db).to_path_buf(),
            });
        }
        Err(source) => {
            return Err(BackupError::Io {
                path: db.to_path_buf(),
                source,
            });
        }
    };
    // Opening the database as another user could leave sidecar files it cannot use, so
    // the owner is checked from the stat alone before the schema read opens it.
    if meta.uid() != uid {
        return Err(BackupError::NotOwner {
            path: db.to_path_buf(),
        });
    }
    let version = snapshot::schema_version(db)?;
    if version < MIN_SCHEMA {
        return Err(BackupError::OldSchema { version });
    }
    Ok(())
}

/// Takes `<tmp>/backup.lock` without waiting; the lock lasts as long as the returned guard.
fn take_lock(tmp: &Path) -> Result<BackupLock, BackupError> {
    let path = tmp.join("backup.lock");
    let io_err = |source| BackupError::Io {
        path: path.clone(),
        source,
    };
    fs::create_dir_all(tmp).map_err(io_err)?;
    // Never truncate: another backup may hold this file.
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_err)?;
    match file.try_lock() {
        Ok(()) => Ok(BackupLock(file)),
        Err(TryLockError::WouldBlock) => Err(BackupError::InProgress),
        Err(TryLockError::Error(source)) => Err(io_err(source)),
    }
}

/// Refuses when the volume's free space is below the size of `repos/` and the database plus
/// the reserve.
fn check_space(data: &Path, db: &Path, reserve: u64) -> Result<(), BackupError> {
    let stat = rustix::fs::statvfs(data).map_err(|e| BackupError::Io {
        path: data.to_path_buf(),
        source: e.into(),
    })?;
    let free = stat.f_bavail.saturating_mul(stat.f_frsize);
    let needed = tree_size(&data.join("repos"))?
        .saturating_add(tree_size(db)?)
        .saturating_add(reserve);
    if free < needed {
        return Err(BackupError::NoSpace { needed, free });
    }
    Ok(())
}

/// The summed length of every entry under `path`, links not followed; zero when absent.
fn tree_size(path: &Path) -> Result<u64, BackupError> {
    let io_err = |source| BackupError::Io {
        path: path.to_path_buf(),
        source,
    };
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(io_err(e)),
    };
    let mut size = meta.len();
    if meta.is_dir() {
        for entry in fs::read_dir(path).map_err(io_err)? {
            size = size.saturating_add(tree_size(&entry.map_err(io_err)?.path())?);
        }
    }
    Ok(size)
}

/// Workspaces left out so far, keyed by (team, ws) so each is named once and sorted.
type LeftOutSet = BTreeMap<(String, String), LeftOutReason>;

/// Copies every live workspace's repository into `<staging>/repos/<team>/<ws>.git`; returns the
/// listing's audit mark and the workspaces skipped. A copy that fails for a workspace whose row
/// is gone was deleted during the backup and is skipped; any other failure aborts.
fn copy_repos(
    data: &Path,
    git: &Path,
    db: &Path,
    staging: &Path,
) -> Result<(snapshot::Mark, LeftOutSet), BackupError> {
    let (rows, mark) = snapshot::live(db)?;
    let mut skipped = LeftOutSet::new();
    for (team, ws) in rows {
        let name = format!("{ws}.git");
        let src = data.join("repos").join(&team).join(&name);
        let parent = staging.join("repos").join(&team);
        let dst = parent.join(&name);
        fs::create_dir_all(&parent).map_err(|source| BackupError::Io {
            path: parent.clone(),
            source,
        })?;
        match copy::copy_repo(git, &src, &dst) {
            Ok(()) => {}
            Err(_) if !snapshot::row_exists(db, &team, &ws)? => {
                remove_dir(&dst)?;
                skipped.insert((team, ws), LeftOutReason::Deleted);
            }
            Err(failed) => {
                return Err(BackupError::Git {
                    workspace: format!("{team}/{ws}"),
                    step: failed.step,
                    stderr: failed.stderr,
                });
            }
        }
    }
    Ok((mark, skipped))
}

/// What each staged repository under `repos` holds, keyed by (team, ws).
fn staged_states(
    git: &Path,
    repos: &Path,
) -> Result<BTreeMap<(String, String), snapshot::StagedRepo>, BackupError> {
    let mut states = BTreeMap::new();
    for team in read_dir(repos)? {
        let team_name = team.file_name().to_string_lossy().into_owned();
        for repo in read_dir(&team.path())? {
            let name = repo.file_name().to_string_lossy().into_owned();
            let Some(ws) = name.strip_suffix(".git") else {
                continue;
            };
            let path = repo.path();
            let state = match copy::has_refs(git, &path) {
                Ok(true) => snapshot::StagedRepo::WithRefs,
                Ok(false) => snapshot::StagedRepo::Empty,
                Err(failed) => {
                    return Err(BackupError::Git {
                        workspace: format!("{team_name}/{ws}"),
                        step: failed.step,
                        stderr: failed.stderr,
                    });
                }
            };
            states.insert((team_name.clone(), ws.to_owned()), state);
        }
    }
    Ok(states)
}

/// Removes each team directory under `repos` that no staged repository is left in.
fn remove_empty_teams(repos: &Path) -> Result<(), BackupError> {
    for team in read_dir(repos)? {
        let path = team.path();
        if read_dir(&path)?.is_empty() {
            fs::remove_dir(&path).map_err(|source| BackupError::Io { path, source })?;
        }
    }
    Ok(())
}

/// The entries of `dir`.
fn read_dir(dir: &Path) -> Result<Vec<fs::DirEntry>, BackupError> {
    let io_err = |source| BackupError::Io {
        path: dir.to_path_buf(),
        source,
    };
    fs::read_dir(dir)
        .map_err(io_err)?
        .collect::<Result<_, _>>()
        .map_err(io_err)
}

/// Removes `dir` and everything under it; an absent `dir` is fine.
fn remove_dir(dir: &Path) -> Result<(), BackupError> {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(BackupError::Io {
            path: dir.to_path_buf(),
            source: e,
        }),
        _ => Ok(()),
    }
}

/// The held backup lock, released when this goes out of scope.
struct BackupLock(File);

impl Drop for BackupLock {
    fn drop(&mut self) {
        // LOCK_UN acts on the lock's open file description, so a forked child's duplicate
        // cannot keep the lock held after the backup returns. Closing the file releases it too.
        let _ = self.0.unlock();
    }
}

/// The staging directory, removed when this goes out of scope on any return.
struct Staging(PathBuf);

impl Staging {
    /// Removes what a crashed run left at `dir`, then creates it empty.
    fn clear(dir: PathBuf) -> Result<Self, BackupError> {
        remove_dir(&dir)?;
        fs::create_dir_all(dir.join("repos")).map_err(|source| BackupError::Io {
            path: dir.clone(),
            source,
        })?;
        Ok(Self(dir))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        // Best effort: a removal that fails here is retried by the next run's `clear`.
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A writer that records whether the wrapped writer itself failed.
struct Sink<'a> {
    inner: &'a mut dyn Write,
    failed: bool,
}

impl Write for Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf).inspect_err(|_| self.failed = true)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().inspect_err(|_| self.failed = true)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    #[test]
    fn another_owner_is_refused_before_the_database_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("server.db");
        let bytes = b"not a sqlite database, and never opened".to_vec();
        std::fs::write(&db, &bytes).unwrap();
        let owner = std::fs::metadata(&db).unwrap().uid();
        let err = refuse_early_as(&db, owner.wrapping_add(1)).unwrap_err();
        assert!(matches!(err, BackupError::NotOwner { .. }), "{err:?}");
        assert_eq!(std::fs::read(&db).unwrap(), bytes, "database untouched");
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["server.db"], "no -wal, -shm or journal");
    }

    #[test]
    fn the_owner_still_gets_the_schema_check() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("server.db");
        rusqlite::Connection::open(&db).unwrap();
        let owner = std::fs::metadata(&db).unwrap().uid();
        let err = refuse_early_as(&db, owner).unwrap_err();
        assert!(
            matches!(err, BackupError::OldSchema { version: 0 }),
            "{err:?}"
        );
    }
}
