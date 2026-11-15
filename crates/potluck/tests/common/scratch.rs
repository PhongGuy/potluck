//! A bare "server" repository and a work tree beside it, driven with the system git through
//! `common::git` (HOME in the temp dir, no global or system config, a fixed identity).

use std::path::{Path, PathBuf};

/// `<dir>/bare.git` (bare, default branch `main`) and `<dir>/work` (no commits yet).
pub struct Scratch {
    pub dir: tempfile::TempDir,
    pub bare: PathBuf,
    pub work: PathBuf,
}

impl Scratch {
    pub fn new() -> Scratch {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("bare.git");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let s = Scratch { dir, bare, work };
        s.git(
            None,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                s.bare.to_str().unwrap(),
            ],
        );
        s.git(Some(&s.work), &["init", "-q", "-b", "main"]);
        s
    }

    /// Runs git in `cwd` (default: the temp dir); panics on failure and returns stdout.
    pub fn git(&self, cwd: Option<&Path>, args: &[&str]) -> String {
        let cwd = cwd.unwrap_or(self.dir.path());
        crate::common::ok(crate::common::git(self.dir.path(), Some(cwd), None, args))
    }

    /// The same in the work tree, trimmed: for `rev-parse` and friends.
    pub fn work_git(&self, args: &[&str]) -> String {
        self.git(Some(&self.work), args).trim().to_owned()
    }

    /// The same against the bare repository, trimmed.
    pub fn bare_git(&self, args: &[&str]) -> String {
        let mut full = vec!["--git-dir", self.bare.to_str().unwrap()];
        full.extend_from_slice(args);
        self.git(None, &full).trim().to_owned()
    }

    pub fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.work.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// `git add -A && git commit`; returns the new HEAD.
    pub fn commit(&self, message: &str) -> String {
        self.git(Some(&self.work), &["add", "-A"]);
        self.git(Some(&self.work), &["commit", "-q", "-m", message]);
        self.work_git(&["rev-parse", "HEAD"])
    }

    /// Puts the work tree's `name` on the server as `refs/heads/<name>`, as an accepted
    /// earlier push would have. `--no-tags`, so no tag rides along.
    pub fn publish(&self, name: &str) {
        let work = self.work.to_str().unwrap().to_owned();
        self.bare_git(&[
            "fetch",
            "-q",
            "--no-tags",
            &work,
            &format!("+{name}:refs/heads/{name}"),
        ]);
    }

    /// Copies the objects behind the work tree's `rev` (a branch name or `refs/tags/<x>`) into
    /// the server with no ref pointing at them: what a pre-receive hook sees. Returns `rev`'s
    /// object name.
    pub fn stage(&self, rev: &str) -> String {
        let work = self.work.to_str().unwrap().to_owned();
        let tmp = format!("refs/staged/{}", rev.replace('/', "-"));
        self.bare_git(&["fetch", "-q", "--no-tags", &work, &format!("+{rev}:{tmp}")]);
        self.bare_git(&["update-ref", "-d", &tmp]);
        self.work_git(&["rev-parse", rev])
    }
}
