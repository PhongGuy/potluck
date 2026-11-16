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

impl Scratch {
    pub fn remove(&self, rel: &str) {
        std::fs::remove_file(self.work.join(rel)).unwrap();
    }

    /// The blob name of `rel` at `rev`.
    pub fn blob(&self, rev: &str, rel: &str) -> String {
        self.work_git(&["rev-parse", &format!("{rev}:{rel}")])
    }

    pub fn branch(&self, name: &str) {
        self.git(Some(&self.work), &["checkout", "-q", "-b", name]);
    }

    pub fn checkout(&self, name: &str) {
        self.git(Some(&self.work), &["checkout", "-q", name]);
    }

    /// A merge commit of `name` into the current branch; returns it.
    pub fn merge(&self, name: &str) -> String {
        self.git(
            Some(&self.work),
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                &format!("merge {name}"),
                name,
            ],
        );
        self.work_git(&["rev-parse", "HEAD"])
    }

    /// A sh wrapper that runs the system git with the same isolation as `common::git`, for
    /// code under test that takes a git binary and passes its own environment through.
    pub fn isolated_git(&self) -> PathBuf {
        let home = self.dir.path().to_str().unwrap();
        self.script(
            "isolated-git",
            &format!(
                "HOME='{home}' GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 exec git \"$@\"\n"
            ),
        )
    }

    /// An executable sh script `name` in the temp dir with `body`.
    pub fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        super::write_script(&path, &format!("#!/bin/sh\n{body}"));
        path
    }

    /// An annotated tag `name` on any object `target` (a commit, tree, blob or tag).
    pub fn tag(&self, name: &str, target: &str) -> String {
        self.git(Some(&self.work), &["tag", "-a", "-m", name, name, target]);
        self.work_git(&["rev-parse", &format!("refs/tags/{name}")])
    }

    /// An annotated tag `name` on `target` whose body is `message`.
    pub fn tag_message(&self, name: &str, target: &str, message: &str) -> String {
        self.git(
            Some(&self.work),
            &["tag", "-a", "-m", message, name, target],
        );
        self.work_git(&["rev-parse", &format!("refs/tags/{name}")])
    }
}

/// Names from `staged_history`.
pub struct History {
    pub tip: String,
    pub merge: String,
    pub root: String,
    pub env_blob: String,
    pub creds_blob: String,
    pub seed_readme: String,
}

/// main: seed (published). Then feat adds environments/dev.yml; main edits README and adds
/// `notes/with space.txt`; merge feat; add creds.txt; remove creds.txt. Staged, not published.
pub fn staged_history(s: &Scratch) -> History {
    s.write("README", b"seed\n");
    let root = s.commit("seed");
    s.publish("main");
    s.branch("feat");
    s.write("environments/dev.yml", b"name: dev\n");
    let env_commit = s.commit("env");
    s.checkout("main");
    s.write("README", b"seed\nmore\n");
    s.write("notes/with space.txt", b"n\n");
    s.commit("more");
    let merge = s.merge("feat");
    s.write("creds.txt", b"AKIAIOSFODNN7EXAMPLE\n");
    let add = s.commit("add creds");
    s.remove("creds.txt");
    s.commit("remove creds");
    let tip = s.stage("main");
    History {
        env_blob: s.blob(&env_commit, "environments/dev.yml"),
        creds_blob: s.blob(&add, "creds.txt"),
        seed_readme: s.blob(&root, "README"),
        tip,
        merge,
        root,
    }
}

impl Scratch {
    /// Merges `name` but leaves the result staged, so the caller can add to the merge commit.
    pub fn merge_no_commit(&self, name: &str) {
        self.git(
            Some(&self.work),
            &["merge", "-q", "--no-ff", "--no-commit", name],
        );
    }

    /// `commit` then `stage("main")`: the new main tip, on the server under no ref.
    pub fn stage_commit(&self, message: &str) -> String {
        self.commit(message);
        self.stage("main")
    }
}
