//! [`Repos`]: where bare repositories live, and how one is created.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::slug::Slug;

/// Bare repositories under `<data>/repos/{team}/{ws}.git`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Repos {
    root: PathBuf,
    git: PathBuf,
}

impl Repos {
    /// `root` is `Config::repos_dir()`, `git` is the binary from `Config::git()`.
    pub fn new(root: PathBuf, git: PathBuf) -> Self {
        Self { root, git }
    }

    /// The git binary this server spawns.
    pub fn git(&self) -> &Path {
        &self.git
    }

    /// `<root>/{team}/{ws}.git`, whether or not it exists.
    pub fn path(&self, team: &Slug, ws: &Slug) -> PathBuf {
        self.root.join(team.as_str()).join(format!("{ws}.git"))
    }

    /// `git init --bare --initial-branch=<default_branch>` at `path(team, ws)`. Creating the
    /// directory is the claim: it either succeeds or fails `AlreadyExists` (`Exists`), so two
    /// concurrent calls cannot both succeed. Returns the repository path.
    pub async fn init(
        &self,
        team: &Slug,
        ws: &Slug,
        default_branch: &str,
    ) -> Result<PathBuf, RepoError> {
        let path = self.path(team, ws);
        let io = |source| RepoError::Io {
            path: path.clone(),
            source,
        };
        tokio::fs::create_dir_all(self.root.join(team.as_str()))
            .await
            .map_err(io)?;
        // Creating the directory is atomic, so it decides which of two racing calls owns the slug;
        // git then initialises inside the empty directory we hold.
        match tokio::fs::create_dir(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(RepoError::Exists { path });
            }
            Err(source) => return Err(io(source)),
        }
        // A clean environment, so the server user's own git config never shapes the repository.
        let mut command = Command::new(&self.git);
        command
            .env_clear()
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("init")
            .arg("-q")
            .arg("--bare")
            .arg(format!("--initial-branch={default_branch}"))
            .arg(&path)
            .stdin(std::process::Stdio::null());
        if let Some(search) = std::env::var_os("PATH") {
            command.env("PATH", search);
        }
        // The directory is ours from the claim; on any failure remove it so a retry starts clean.
        let failure = match command.output().await {
            Ok(output) if output.status.success() => return Ok(path),
            Ok(output) => RepoError::GitInit {
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                path: path.clone(),
            },
            Err(source) => io(source),
        };
        let _ = tokio::fs::remove_dir_all(&path).await;
        Err(failure)
    }

    /// Removes `path(team, ws)` and everything under it; `Ok(false)` when it was not there.
    pub async fn remove(&self, team: &Slug, ws: &Slug) -> Result<bool, RepoError> {
        let path = self.path(team, ws);
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(RepoError::Io { path, source }),
        }
    }
}

/// Why a repository could not be created.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RepoError {
    /// The directory is already there.
    #[error("repository {path} already exists")]
    Exists {
        /// The repository path.
        path: PathBuf,
    },
    /// Creating the parent directory or spawning git failed.
    #[error("cannot create {path}: {source}")]
    Io {
        /// The repository path.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// `git init` exited non-zero.
    #[error("git init failed for {path}: {stderr}")]
    GitInit {
        /// The repository path.
        path: PathBuf,
        /// git's stderr, trimmed.
        stderr: String,
    },
}
