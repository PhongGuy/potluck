//! Online backup: a consistent copy of a running server's data directory, streamed as a tar.
//!
//! Each workspace repository is copied by a local `git fetch` into a staging directory, the
//! database is snapshotted with SQLite's backup API, and the staged tree is written as a tar.

mod archive;
#[cfg_attr(not(test), expect(dead_code, reason = "wired by write_backup"))]
mod copy;
#[cfg_attr(not(test), expect(dead_code, reason = "wired by write_backup"))]
mod snapshot;

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
/// keeps running. Refuses before copying anything when there is no database, its schema is
/// older than 3, this user does not own it, another backup holds `<data>/tmp/backup.lock`, or
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
    let workspaces = copy_repos(data, git, &db, &staging.0)?;
    on_phase(Phase::ReposCopied);
    snapshot::snapshot(&db, &staging.0.join("server.db"))?;
    on_phase(Phase::DbSnapshotted);

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
        workspaces,
        ..Written::default()
    })
}

/// The refusals that need no lock: no database, an old schema, another owner.
fn refuse_early(db: &Path) -> Result<(), BackupError> {
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
    let version = snapshot::schema_version(db)?;
    if version < MIN_SCHEMA {
        return Err(BackupError::OldSchema { version });
    }
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Err(BackupError::NotOwner {
            path: db.to_path_buf(),
        });
    }
    Ok(())
}

/// Takes `<tmp>/backup.lock` without waiting; the lock lasts as long as the returned file.
fn take_lock(tmp: &Path) -> Result<File, BackupError> {
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
        Ok(()) => Ok(file),
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

/// Copies every live workspace's repository into `<staging>/repos/<team>/<ws>.git`; returns how
/// many were staged. A copy that fails for a workspace whose row is gone was deleted during the
/// backup and is skipped; any other failure aborts.
fn copy_repos(data: &Path, git: &Path, db: &Path, staging: &Path) -> Result<usize, BackupError> {
    let (rows, _mark) = snapshot::live(db)?;
    let mut staged = 0;
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
            Ok(()) => staged += 1,
            Err(_) if !snapshot::row_exists(db, &team, &ws)? => remove_dir(&dst)?,
            Err(failed) => {
                return Err(BackupError::Git {
                    workspace: format!("{team}/{ws}"),
                    step: failed.step,
                    stderr: failed.stderr,
                });
            }
        }
    }
    Ok(staged)
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
