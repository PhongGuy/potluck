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

    /// `git init --bare --initial-branch=<default_branch>` at `path(team, ws)`; refuses an
    /// existing directory. Returns the repository path.
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
        if tokio::fs::try_exists(&path).await.map_err(io)? {
            return Err(RepoError::Exists { path });
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
        let output = command.output().await.map_err(io)?;
        if !output.status.success() {
            // git can leave an empty directory behind; remove it so a retry starts clean.
            let _ = tokio::fs::remove_dir_all(&path).await;
            return Err(RepoError::GitInit {
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                path,
            });
        }
        Ok(path)
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
