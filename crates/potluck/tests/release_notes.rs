//! `scripts/release.sh` against a throwaway git repository and a stub `gh`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::common::repo_root;

const IMAGE: &str = "ghcr.io/phongguy/potluck";
const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn script() -> PathBuf {
    repo_root().join("scripts/release.sh")
}

/// Git in a throwaway repo, blind to the developer's global and system config and hooks.
fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repo with v0.1.0 at its first commit and, after `subjects`, HEAD tagged `tag`.
fn repo_with(subjects: &[&str], tag: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    git(
        repo,
        &["commit", "-q", "--allow-empty", "-m", "chore: start"],
    );
    git(repo, &["tag", "v0.1.0"]);
    for subject in subjects {
        git(repo, &["commit", "-q", "--allow-empty", "-m", subject]);
    }
    git(repo, &["tag", tag]);
    dir
}

/// A `gh` stub that records its arguments and prints a canned generate-notes body.
fn stub_gh(dir: &Path) -> PathBuf {
    let path = dir.join("gh");
    fs::write(
        &path,
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" > \"$(dirname \"$0\")/gh-args\"\n\
         printf \"## What's Changed\\n* Add thing by @someone in #1\\n\\n\\n**Full Changelog**: x\\n\"\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn notes(repo: &Path, tag: &str) -> (Output, String) {
    let out_file = repo.join("notes.md");
    let gh = stub_gh(repo);
    let out = Command::new(script())
        .current_dir(repo)
        .env("GH", &gh)
        .args(["notes", tag, "PhongGuy/potluck", DIGEST])
        .arg(&out_file)
        .output()
        .unwrap();
    let text = fs::read_to_string(&out_file).unwrap_or_default();
    (out, text)
}

fn heading_order(text: &str) -> Vec<&str> {
    text.lines().filter(|l| l.starts_with("## ")).collect()
}

#[test]
fn version_strips_the_v_and_refuses_other_tags() {
    let ok = Command::new(script())
        .args(["version", "v1.2.3"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&ok.stdout), "1.2.3\n");
    let pre = Command::new(script())
        .args(["version", "v1.2.3-rc.1"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&pre.stdout), "1.2.3-rc.1\n");
    let bad = Command::new(script())
        .args(["version", "release-1"])
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(1));
}

#[test]
fn notes_follow_the_layout_and_count_user_facing_changes() {
    let dir = repo_with(
        &[
            "feat(api): add teams",
            "fix(db): close on drop",
            "fix(ci): pin actions",
            "feat: two",
        ],
        "v0.2.0",
    );
    let (out, text) = notes(dir.path(), "v0.2.0");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.starts_with("This release adds 2 features and fixes 1 bug.\n"),
        "{text}"
    );
    assert_eq!(
        heading_order(&text),
        [
            "## What's Changed",
            "## Downloads",
            "## Before you install",
            "## Updates"
        ]
    );
    assert!(text.contains("* Add thing by @someone in #1\n"), "{text}");
    assert_eq!(text.matches("## What's Changed").count(), 1);
    assert!(text.contains(
        "**Full Changelog**: https://github.com/PhongGuy/potluck/compare/v0.1.0...v0.2.0\n"
    ));
    assert!(
        text.contains(&format!("| Linux x86_64 and arm64 | `{IMAGE}:v0.2.0` |")),
        "{text}"
    );
    assert!(text.contains(&format!("`{IMAGE}@{DIGEST}`")), "{text}");
    assert!(
        !text.contains("gh attestation verify"),
        "unattested notes name no check: {text}"
    );
    let args = fs::read_to_string(dir.path().join("gh-args")).unwrap();
    assert!(
        args.contains("tag_name=v0.2.0") && args.contains("previous_tag_name=v0.1.0"),
        "{args}"
    );
}

#[test]
fn the_attestation_check_appears_only_when_attested() {
    let dir = repo_with(&["feat: x"], "v0.2.0");
    let gh = stub_gh(dir.path());
    let out_file = dir.path().join("attested.md");
    let out = Command::new(script())
        .current_dir(dir.path())
        .env("GH", &gh)
        .env("ATTESTED", "true")
        .args(["notes", "v0.2.0", "PhongGuy/potluck", DIGEST])
        .arg(&out_file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = fs::read_to_string(&out_file).unwrap();
    assert!(
        text.contains(&format!(
            "`gh attestation verify oci://{IMAGE}:v0.2.0 --repo PhongGuy/potluck`"
        )),
        "{text}"
    );
}

#[test]
fn a_first_release_has_no_previous_tag() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(
        dir.path(),
        &["commit", "-q", "--allow-empty", "-m", "feat: begin"],
    );
    git(dir.path(), &["tag", "v0.1.0"]);
    let (out, text) = notes(dir.path(), "v0.1.0");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.starts_with("The first release of Potluck.\n"),
        "{text}"
    );
    assert!(!text.contains("**Full Changelog**"), "{text}");
    let args = fs::read_to_string(dir.path().join("gh-args")).unwrap();
    assert!(!args.contains("previous_tag_name"), "{args}");
}

#[test]
fn a_pre_release_tag_is_never_the_previous_release() {
    let dir = repo_with(&["feat: rc work"], "v0.2.0-rc.1");
    git(
        dir.path(),
        &["commit", "-q", "--allow-empty", "-m", "fix: after rc"],
    );
    git(dir.path(), &["tag", "v0.2.0"]);
    let (_, text) = notes(dir.path(), "v0.2.0");
    assert!(text.contains("compare/v0.1.0...v0.2.0"), "{text}");
    assert!(
        text.starts_with("This release adds 1 feature and fixes 1 bug.\n"),
        "{text}"
    );
}

#[test]
fn an_empty_generate_notes_reply_fails() {
    let dir = repo_with(&["feat: x"], "v0.2.0");
    let gh = dir.path().join("gh");
    fs::write(&gh, "#!/usr/bin/env bash\nexit 0\n").unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(script())
        .current_dir(dir.path())
        .env("GH", &gh)
        .args(["notes", "v0.2.0", "PhongGuy/potluck", DIGEST, "notes.md"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("generate-notes"));
}
