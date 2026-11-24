//! Repository copy: a clean-environment git and a local fetch of every ref into a new bare repository.

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A git step that exited non-zero or could not start; `stderr` is git's, trimmed.
#[derive(Debug)]
pub(crate) struct GitFailed {
    pub(crate) step: &'static str,
    pub(crate) stderr: String,
}

/// `git` with an empty environment plus PATH, GIT_CONFIG_GLOBAL=/dev/null and
/// GIT_CONFIG_NOSYSTEM=1; stdin null, stdout and stderr piped.
pub(crate) fn git_command(git: &Path) -> Command {
    // The same clean environment as `Repos::init`, so the server user's git config never applies.
    let mut command = Command::new(git);
    command
        .env_clear()
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(search) = std::env::var_os("PATH") {
        command.env("PATH", search);
    }
    command
}

/// Reads `src`'s HEAD branch, `init --bare --initial-branch=<it>` at `dst` (absent), then
/// `-c fetch.fsckObjects=true -c gc.auto=0 fetch -q --no-auto-gc --no-write-fetch-head <src> '+refs/*:refs/*'`.
pub(crate) fn copy_repo(git: &Path, src: &Path, dst: &Path) -> Result<(), GitFailed> {
    let head = run(
        "symbolic-ref",
        git_command(git)
            .arg(git_dir(src))
            .args(["symbolic-ref", "HEAD"]),
    )?;
    let head = String::from_utf8_lossy(&head.stdout);
    let head = head.trim();
    let branch = head.strip_prefix("refs/heads/").unwrap_or(head);
    run(
        "init",
        git_command(git)
            .args(["init", "-q", "--bare"])
            .arg(format!("--initial-branch={branch}"))
            .arg(dst),
    )?;
    // Fetching by path names no remote, so the copy's config stays clean; fsck on receipt
    // catches a corrupt source object, and no gc runs in the copy while it is being staged.
    run(
        "fetch",
        git_command(git)
            .arg(git_dir(dst))
            .args(["-c", "fetch.fsckObjects=true", "-c", "gc.auto=0"])
            .args(["fetch", "-q", "--no-auto-gc", "--no-write-fetch-head"])
            .arg(src)
            .arg("+refs/*:refs/*"),
    )?;
    Ok(())
}

/// True when `repo` has at least one ref.
pub(crate) fn has_refs(git: &Path, repo: &Path) -> Result<bool, GitFailed> {
    let out = run(
        "for-each-ref",
        git_command(git).arg(git_dir(repo)).args([
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
        ]),
    )?;
    Ok(!out.stdout.is_empty())
}

fn git_dir(repo: &Path) -> std::ffi::OsString {
    let mut arg = std::ffi::OsString::from("--git-dir=");
    arg.push(repo);
    arg
}

/// Runs `command` to completion; a spawn error or a non-zero exit becomes `GitFailed` at `step`.
fn run(step: &'static str, command: &mut Command) -> Result<Output, GitFailed> {
    let out = command.output().map_err(|e| GitFailed {
        step,
        stderr: e.to_string(),
    })?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(GitFailed {
            step,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    const GIT: &str = "git";

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = git_command(Path::new(GIT))
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// `<dir>/src.git` on branch `trunk`: two commits, an annotated tag and a branch `side`.
    fn source(dir: &Path) -> PathBuf {
        let src = dir.join("src.git");
        run(
            dir,
            &[
                "init",
                "-q",
                "--bare",
                "--initial-branch=trunk",
                src.to_str().unwrap(),
            ],
        );
        let work = dir.join("work");
        run(dir, &["init", "-q", "-b", "trunk", work.to_str().unwrap()]);
        std::fs::write(work.join("a"), "one\n").unwrap();
        run(&work, &["add", "a"]);
        run(&work, &["commit", "-q", "-m", "one"]);
        run(&work, &["tag", "-a", "-m", "v1", "v1"]);
        run(&work, &["branch", "side"]);
        std::fs::write(work.join("a"), "two\n").unwrap();
        run(&work, &["commit", "-q", "-am", "two"]);
        run(
            &work,
            &["push", "-q", src.to_str().unwrap(), "trunk", "side", "v1"],
        );
        src
    }

    fn refs(repo: &Path) -> String {
        run(
            repo,
            &[
                "--git-dir=.",
                "for-each-ref",
                "--format=%(objectname) %(refname)",
            ],
        )
    }

    #[test]
    fn a_copy_carries_every_ref_and_the_head_branch_and_no_remote() {
        let dir = tempfile::tempdir().unwrap();
        let src = source(dir.path());
        let dst = dir.path().join("dst.git");
        copy_repo(Path::new(GIT), &src, &dst).unwrap();
        assert_eq!(refs(&dst), refs(&src), "same refs");
        assert_eq!(
            run(&dst, &["--git-dir=.", "symbolic-ref", "HEAD"]).trim(),
            "refs/heads/trunk",
            "HEAD"
        );
        run(&dst, &["--git-dir=.", "fsck", "--no-progress"]);
        let config = std::fs::read_to_string(dst.join("config")).unwrap();
        assert!(!config.contains("[remote"), "no remote: {config}");
        assert!(has_refs(Path::new(GIT), &dst).unwrap(), "has refs");
    }

    #[test]
    fn an_empty_source_copies_to_an_empty_repository() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("empty.git");
        run(
            dir.path(),
            &[
                "init",
                "-q",
                "--bare",
                "--initial-branch=main",
                src.to_str().unwrap(),
            ],
        );
        let dst = dir.path().join("dst.git");
        copy_repo(Path::new(GIT), &src, &dst).unwrap();
        assert!(!has_refs(Path::new(GIT), &dst).unwrap(), "no refs");
    }

    #[test]
    fn a_missing_source_fails() {
        let dir = tempfile::tempdir().unwrap();
        let err = copy_repo(
            Path::new(GIT),
            &dir.path().join("gone.git"),
            &dir.path().join("dst.git"),
        );
        assert!(err.is_err(), "missing source must fail");
    }

    #[test]
    fn a_corrupt_source_object_fails_the_copy() {
        let dir = tempfile::tempdir().unwrap();
        let src = source(dir.path());
        let blob = run(&dir.path().join("work"), &["rev-parse", "HEAD:a"]);
        let blob = blob.trim();
        let loose = src.join("objects").join(&blob[..2]).join(&blob[2..]);
        assert!(
            loose.exists(),
            "a small push stays loose: {}",
            loose.display()
        );
        let mut perms = std::fs::metadata(&loose).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o644);
        std::fs::set_permissions(&loose, perms).unwrap();
        std::fs::write(&loose, b"not a zlib stream").unwrap();
        let err = copy_repo(Path::new(GIT), &src, &dir.path().join("dst.git"));
        assert!(err.is_err(), "a corrupt object must fail the fetch");
    }

    #[test]
    fn a_gc_running_in_the_source_does_not_break_the_copy() {
        let dir = tempfile::tempdir().unwrap();
        let src = source(dir.path());
        let expected = refs(&src);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gc = {
            let (src, stop) = (src.clone(), std::sync::Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    run(&src, &["--git-dir=.", "gc", "-q", "--prune=now"]);
                }
            })
        };
        for i in 0..5 {
            let dst = dir.path().join(format!("dst{i}.git"));
            copy_repo(Path::new(GIT), &src, &dst).unwrap();
            assert_eq!(refs(&dst), expected, "copy {i} refs");
            run(&dst, &["--git-dir=.", "fsck", "--no-progress"]);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        gc.join().unwrap();
    }
}
