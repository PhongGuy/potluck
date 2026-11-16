//! [`Repos`]: where bare repositories live, and how one is created.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::git::engine::{DEFAULT_MAX_RUNTIME_SECS, GroupKill, kill_and_reap, process_group};
use crate::slug::Slug;

/// The author and committer date of every seed commit, `GIT_*_DATE` form: 2026-01-01T00:00:00Z.
/// The same seed on the same host is the same commit on every server.
pub const SEED_DATE: &str = "1767225600 +0000";

/// Bare repositories under `<data>/repos/{team}/{ws}.git`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Repos {
    root: PathBuf,
    git: PathBuf,
    max_runtime: Duration,
}

impl Repos {
    /// `root` is `Config::repos_dir()`, `git` is the binary from `Config::git()`.
    pub fn new(root: PathBuf, git: PathBuf) -> Self {
        Self {
            root,
            git,
            max_runtime: Duration::from_secs(DEFAULT_MAX_RUNTIME_SECS),
        }
    }

    /// The same layout with each git child allowed `max_runtime` from spawn; default 30 min.
    #[must_use]
    pub fn with_max_runtime(self, max_runtime: Duration) -> Self {
        Self {
            max_runtime,
            ..self
        }
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
            .arg(&path);
        if let Some(search) = std::env::var_os("PATH") {
            command.env("PATH", search);
        }
        // The directory is ours from the claim; on any failure remove it so a retry starts clean.
        let failure = match self.run(command, &path, "init", &[]).await {
            Ok(output) if output.status.success() => return Ok(path),
            Ok(output) => RepoError::GitInit {
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                path: path.clone(),
            },
            Err(err) => err,
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

    /// One root commit on `refs/heads/<branch>` of the bare repository at `repo`, holding
    /// exactly `files` (root-level `(name, bytes)` pairs, mode 100644) as `author <email>`,
    /// dated `SEED_DATE`, with `message`. Refuses a name that is empty, `.` or `..`, carries
    /// `/`, `\n` or `\t`, or repeats an earlier one (`SeedPath`) before spawning anything, and
    /// a branch that already exists (`Seed` at step `update-ref`). Returns the commit sha.
    pub async fn seed_commit(
        &self,
        repo: &Path,
        branch: &str,
        files: &[(&str, &[u8])],
        author: &str,
        email: &str,
        message: &str,
    ) -> Result<String, RepoError> {
        // `mktree` input is tab- and line-delimited, takes no nesting and writes a repeated name
        // as a tree fsck rejects, so a bad name is refused before any plumbing runs.
        let mut seen = std::collections::HashSet::new();
        if let Some((name, _)) = files.iter().find(|(name, _)| {
            name.is_empty()
                || *name == "."
                || *name == ".."
                || name.contains(['/', '\n', '\t'])
                || !seen.insert(*name)
        }) {
            return Err(RepoError::SeedPath {
                name: (*name).to_owned(),
            });
        }
        let mut tree = String::new();
        for (name, bytes) in files {
            let blob = self
                .plumb(
                    repo,
                    "hash-object",
                    &["hash-object", "-w", "--stdin"],
                    bytes,
                    &[],
                )
                .await?;
            tree.push_str(&format!("100644 blob {blob}\t{name}\n"));
        }
        let tree = self
            .plumb(repo, "mktree", &["mktree"], tree.as_bytes(), &[])
            .await?;
        let mut message = message.to_owned();
        if !message.ends_with('\n') {
            message.push('\n');
        }
        // A fixed identity and date make the commit a pure function of files, host and kind.
        let identity = [
            ("GIT_AUTHOR_NAME", author),
            ("GIT_AUTHOR_EMAIL", email),
            ("GIT_AUTHOR_DATE", SEED_DATE),
            ("GIT_COMMITTER_NAME", author),
            ("GIT_COMMITTER_EMAIL", email),
            ("GIT_COMMITTER_DATE", SEED_DATE),
        ];
        let commit = self
            .plumb(
                repo,
                "commit-tree",
                &["commit-tree", &tree],
                message.as_bytes(),
                &identity,
            )
            .await?;
        // The all-zero old value makes git refuse when the branch already exists.
        let reference = format!("refs/heads/{branch}");
        self.plumb(
            repo,
            "update-ref",
            &[
                "update-ref",
                &reference,
                &commit,
                "0000000000000000000000000000000000000000",
            ],
            &[],
            &[],
        )
        .await?;
        Ok(commit)
    }

    /// Runs `git --git-dir=<repo> <args>` in the clean environment `init` uses, feeding it
    /// `stdin`; returns trimmed stdout, or `Seed { step, .. }` on a non-zero exit.
    async fn plumb(
        &self,
        repo: &Path,
        step: &'static str,
        args: &[&str],
        stdin: &[u8],
        env: &[(&str, &str)],
    ) -> Result<String, RepoError> {
        let mut command = Command::new(&self.git);
        command
            .env_clear()
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .envs(env.iter().copied())
            .arg(format!("--git-dir={}", repo.display()))
            .args(args);
        if let Some(search) = std::env::var_os("PATH") {
            command.env("PATH", search);
        }
        let output = self.run(command, repo, step, stdin).await?;
        if !output.status.success() {
            return Err(RepoError::Seed {
                path: repo.to_path_buf(),
                step,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Spawns `command` in its own process group, feeds it `stdin` and collects its output. A git
    /// still running `max_runtime` after spawn has its whole group killed and reaped: `Overran`.
    pub(super) async fn run(
        &self,
        mut command: Command,
        path: &Path,
        step: &'static str,
        stdin: &[u8],
    ) -> Result<Output, RepoError> {
        let io = |source| RepoError::Io {
            path: path.to_path_buf(),
            source,
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(io)?;
        let group = process_group(&child);
        let guard = GroupKill(group);
        let (Some(mut input), Some(mut stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(io(std::io::Error::other("git has no pipes")));
        };
        // Feed stdin while draining both pipes, and reap, all inside the run limit; dropping
        // the input once written gives git its EOF.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let bounded = tokio::time::timeout(self.max_runtime, async {
            let feed = async move { input.write_all(stdin).await };
            let (fed, read_out, read_err) = tokio::join!(
                feed,
                stdout.read_to_end(&mut out),
                stderr.read_to_end(&mut err)
            );
            (fed, read_out, read_err, child.wait().await)
        })
        .await;
        let Ok((fed, read_out, read_err, status)) = bounded else {
            kill_and_reap(&mut child, group).await;
            guard.disarm();
            tracing::warn!(step, limit = ?self.max_runtime, "git ran past its run limit; killed its process group");
            return Err(RepoError::Overran {
                path: path.to_path_buf(),
                step,
                limit: self.max_runtime,
            });
        };
        if status.is_ok() {
            guard.disarm();
        }
        // A git that already exited closes stdin early; its exit status then tells what happened.
        match fed {
            Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => return Err(io(e)),
            _ => {}
        }
        read_out.map_err(io)?;
        read_err.map_err(io)?;
        Ok(Output {
            status: status.map_err(io)?,
            stdout: out,
            stderr: err,
        })
    }
}

/// Why creating, seeding or removing a repository failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RepoError {
    /// The directory is already there.
    #[error("repository {path} already exists")]
    Exists {
        /// The repository path.
        path: PathBuf,
    },
    /// A filesystem step on the repository, or spawning or feeding git, failed.
    #[error("i/o on repository {path} failed: {source}")]
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
    /// A plumbing step of `seed_commit` exited non-zero.
    #[error("seeding {path} failed at `git {step}`: {stderr}")]
    Seed {
        /// The repository path.
        path: PathBuf,
        /// `hash-object`, `mktree`, `commit-tree` or `update-ref`.
        step: &'static str,
        /// git's stderr, trimmed.
        stderr: String,
    },
    /// git ran past its run limit while making the repository; its process group was killed.
    #[error("git {step} for {path} ran past its run limit of {limit:?} and was killed")]
    Overran {
        /// The repository path.
        path: PathBuf,
        /// `init`, or the `seed_commit` plumbing step that hung.
        step: &'static str,
        /// The run limit git exceeded.
        limit: Duration,
    },
    /// A seed file name is empty, `.`, `..`, carries `/`, a newline or a tab, or is repeated.
    #[error("seed file name {name:?} must be a plain root-level file name")]
    SeedPath {
        /// The offending name.
        name: String,
    },
}
