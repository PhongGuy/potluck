//! `scripts/release.sh` against a throwaway git repository and a stub `gh`.

use std::fs;
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
    crate::common::write_script(
        &path,
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" > \"$(dirname \"$0\")/gh-args\"\n\
         printf \"## What's Changed\\n* Add thing by @someone in #1\\n\\n\\n**Full Changelog**: x\\n\"\n",
    );
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
fn version_refuses_leading_zeros_in_any_core_number() {
    for tag in ["v01.2.3", "v1.02.3", "v1.2.03", "v00.0.0", "v01.2.3-rc.1"] {
        let out = Command::new(script())
            .args(["version", tag])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{tag} is refused");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "",
            "{tag} prints no version"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(tag),
            "{tag}: the message names the tag: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    for (tag, version) in [("v0.0.0", "0.0.0"), ("v10.20.30", "10.20.30")] {
        let out = Command::new(script())
            .args(["version", tag])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("{version}\n"),
            "{tag} is accepted"
        );
    }
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
fn attested_notes_name_the_manifest_file_check_and_unattested_notes_name_none() {
    const FILES: &str = "Check the signed release manifest the same way with `gh attestation verify potluck-release.json --repo PhongGuy/potluck` and `gh attestation verify potluck-release.json.sig --repo PhongGuy/potluck`.";
    for attested in [true, false] {
        let dir = repo_with(&["feat: x"], "v0.2.0");
        let gh = stub_gh(dir.path());
        let out_file = dir.path().join("notes-out.md");
        let mut cmd = Command::new(script());
        cmd.current_dir(dir.path())
            .env("GH", &gh)
            .env_remove("ATTESTED");
        if attested {
            cmd.env("ATTESTED", "true");
        }
        let out = cmd
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
        if attested {
            assert!(text.lines().any(|l| l == FILES), "{text}");
        } else {
            assert!(!text.contains("gh attestation"), "{text}");
        }
    }
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
    crate::common::write_script(&gh, "#!/usr/bin/env bash\nexit 0\n");
    let out = Command::new(script())
        .current_dir(dir.path())
        .env("GH", &gh)
        .args(["notes", "v0.2.0", "PhongGuy/potluck", DIGEST, "notes.md"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("generate-notes"));
}

#[test]
fn only_a_final_release_says_it_is_tagged_latest() {
    let final_dir = repo_with(&["feat: x"], "v0.2.0");
    let (out, text) = notes(final_dir.path(), "v0.2.0");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("also tagged `latest`"), "{text}");

    let rc_dir = repo_with(&["feat: x"], "v0.2.0-rc.1");
    let (out, text) = notes(rc_dir.path(), "v0.2.0-rc.1");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains(&format!("`docker pull {IMAGE}:v0.2.0-rc.1`")),
        "{text}"
    );
    assert!(
        !text.contains("latest"),
        "a pre-release never moves latest: {text}"
    );
}

/// Run `release.sh` with `args` from `cwd`.
fn release_sh(cwd: &Path, args: &[&str]) -> Output {
    Command::new(script())
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

/// True for a UTC timestamp written exactly as `YYYY-MM-DDTHH:MM:SSZ`.
fn utc_seconds(text: &str) -> bool {
    text.len() == 20
        && text.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            10 => b == b'T',
            13 | 16 => b == b':',
            19 => b == b'Z',
            _ => b.is_ascii_digit(),
        })
}

#[test]
fn manifest_writes_the_five_fields_for_a_final_or_pre_release_tag() {
    let dir = tempfile::tempdir().unwrap();
    for (tag, version) in [("v1.2.0", "1.2.0"), ("v1.2.0-rc.1", "1.2.0-rc.1")] {
        let out_file = dir.path().join(format!("{tag}.json"));
        let out = release_sh(
            dir.path(),
            &["manifest", tag, DIGEST, out_file.to_str().unwrap()],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&out_file).unwrap()).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["digest", "image", "notes_url", "published", "version"]
        );
        assert_eq!(json["version"], version);
        assert_eq!(json["image"], IMAGE);
        assert_eq!(json["digest"], DIGEST);
        assert_eq!(
            json["notes_url"],
            format!("https://github.com/PhongGuy/potluck/releases/tag/{tag}")
        );
        let published = json["published"].as_str().unwrap();
        assert!(utc_seconds(published), "{published}");
    }
}

#[test]
fn manifest_refuses_a_malformed_digest_or_tag_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let hex = DIGEST.strip_prefix("sha256:").unwrap();
    let cases = [
        ("v1.2.0", hex.to_owned(), "digest"),
        ("v1.2.0", format!("sha256:{}", &hex[1..]), "digest"),
        ("v1.2.0", format!("{DIGEST}0"), "digest"),
        ("v1.2.0", format!("sha256:{}", hex.to_uppercase()), "digest"),
        ("v1.2.0", format!("sha512:{hex}"), "digest"),
        ("v1.2.0", format!("{DIGEST}\n"), "digest"),
        ("v1.2.0", String::new(), "digest"),
        ("release-1", DIGEST.to_owned(), "release-1"),
    ];
    for (i, (tag, digest, named)) in cases.iter().enumerate() {
        let out_file = dir.path().join(format!("{i}.json"));
        let out = release_sh(
            dir.path(),
            &["manifest", tag, digest, out_file.to_str().unwrap()],
        );
        assert_eq!(out.status.code(), Some(1), "{tag} {digest:?} is refused");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(named),
            "{tag} {digest:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out_file.exists(), "{tag} {digest:?} wrote a manifest");
    }
}

/// A Cargo.toml whose only `[workspace.package]` version is 1.2.0, with decoy versions in
/// the other sections a careless reader could pick up.
const WORKSPACE_TOML: &str = "[package]\nname = \"decoy\"\nversion = \"9.9.9\"\n\n\
[workspace]\nmembers = [\"a\"]\n\n\
[workspace.package]\nedition = \"2024\"\nversion = \"1.2.0\" # the release version\n\n\
[workspace.dependencies]\nversion = \"7.7.7\"\nfoo = { version = \"=3.0.0\" }\n";

#[test]
fn check_version_accepts_only_the_workspace_package_version() {
    let dir = tempfile::tempdir().unwrap();
    let toml = dir.path().join("Cargo.toml");
    fs::write(&toml, WORKSPACE_TOML).unwrap();
    let toml = toml.to_str().unwrap();
    let ok = release_sh(dir.path(), &["check-version", "v1.2.0", toml]);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&ok.stdout), "");
    for tag in ["v1.2.1", "v1.2.0-rc.1", "v9.9.9", "v7.7.7", "v3.0.0"] {
        let out = release_sh(dir.path(), &["check-version", tag, toml]);
        assert_eq!(out.status.code(), Some(1), "{tag} is refused");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(tag) && err.contains("1.2.0"),
            "{tag}: the refusal names both versions: {err}"
        );
    }
}

#[test]
fn check_version_refuses_a_bad_tag_or_a_missing_version() {
    let dir = tempfile::tempdir().unwrap();
    let no_version = dir.path().join("no-version.toml");
    fs::write(
        &no_version,
        "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nedition = \"2024\"\n",
    )
    .unwrap();
    let missing = dir.path().join("missing.toml");
    let good = dir.path().join("Cargo.toml");
    fs::write(&good, WORKSPACE_TOML).unwrap();
    for (tag, toml, named) in [
        ("v1.2.0", &no_version, "no-version.toml"),
        ("v1.2.0", &missing, "missing.toml"),
        ("release-1", &good, "release-1"),
    ] {
        let out = release_sh(dir.path(), &["check-version", tag, toml.to_str().unwrap()]);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{tag} {} is refused",
            toml.display()
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(named),
            "{named}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A `tomllib` that reports 6.6.6 for any file, for python3 to pick up if it is not isolated.
const DECOY_TOMLLIB: &str =
    "def load(f):\n    return {\"workspace\": {\"package\": {\"version\": \"6.6.6\"}}}\n";

#[test]
fn check_version_ignores_a_tomllib_in_the_cwd_or_on_pythonpath() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("tomllib.py"), DECOY_TOMLLIB).unwrap();
    let toml = dir.path().join("Cargo.toml");
    fs::write(&toml, WORKSPACE_TOML).unwrap();
    let out = Command::new(script())
        .current_dir(dir.path())
        .env("PYTHONPATH", dir.path())
        .args(["check-version", "v1.2.0", toml.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "a decoy tomllib was read: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_version_names_python3_when_it_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    // A PATH holding only bash, so the script starts but finds no python3.
    let bash = Command::new("bash")
        .args(["-c", "command -v bash"])
        .output()
        .unwrap();
    let bash = String::from_utf8(bash.stdout).unwrap();
    std::os::unix::fs::symlink(bash.trim(), bin.join("bash")).unwrap();
    let toml = dir.path().join("Cargo.toml");
    fs::write(&toml, WORKSPACE_TOML).unwrap();
    let out = Command::new(script())
        .current_dir(dir.path())
        .env("PATH", &bin)
        .args(["check-version", "v1.2.0", toml.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("python3"),
        "the refusal does not name python3: {err}"
    );
    assert!(!err.contains("no [workspace.package] version"), "{err}");
}

#[test]
fn check_version_defaults_to_the_repository_cargo_toml() {
    // Run from an unrelated directory: the default path follows the script, not the cwd.
    let dir = tempfile::tempdir().unwrap();
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    let ok = release_sh(dir.path(), &["check-version", &tag]);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let off = release_sh(dir.path(), &["check-version", "v999.0.0"]);
    assert_eq!(off.status.code(), Some(1));
}

/// The Downloads line naming the signed manifest the server's update check fetches.
const MANIFEST_LINE: &str = "The signed release manifest, `potluck-release.json` with its minisign signature `potluck-release.json.sig`, is attached below; servers verify the signature before they announce a release.";

#[test]
fn downloads_name_the_signed_manifest_for_final_and_pre_releases() {
    for tag in ["v0.2.0", "v0.2.0-rc.1"] {
        let dir = repo_with(&["feat: x"], tag);
        let (out, text) = notes(dir.path(), tag);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let downloads = text
            .split("## Downloads\n")
            .nth(1)
            .and_then(|rest| rest.split("\n## ").next())
            .unwrap_or_else(|| panic!("no Downloads section: {text}"));
        assert!(
            downloads.lines().any(|l| l == MANIFEST_LINE),
            "{tag}: {downloads}"
        );
    }
}
