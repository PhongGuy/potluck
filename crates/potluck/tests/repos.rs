//! `Repos` creates and locates bare repositories under a temp root.

use std::path::{Path, PathBuf};

use potluck::git::{RepoError, Repos};
use potluck::slug::Slug;

use crate::common::{SEED_COMMIT, git, hanging_git, ok};

fn repos(root: &Path) -> Repos {
    Repos::new(root.to_path_buf(), PathBuf::from("git"))
}

fn slug(s: &str) -> Slug {
    Slug::parse(s).unwrap()
}

#[test]
fn path_is_root_team_ws_dot_git() {
    let r = repos(Path::new("/srv/potluck/repos"));
    assert_eq!(
        r.path(&slug("core"), &slug("api")),
        Path::new("/srv/potluck/repos/core/api.git")
    );
    assert_eq!(r.git(), Path::new("git"));
}

#[tokio::test]
async fn init_creates_a_bare_repository_with_the_default_branch() {
    let dir = tempfile::tempdir().unwrap();
    let r = repos(dir.path());
    let path = r.init(&slug("core"), &slug("api"), "main").await.unwrap();
    assert_eq!(path, dir.path().join("core").join("api.git"));
    assert!(
        path.join("HEAD").is_file(),
        "a bare repo has HEAD at its root"
    );
    assert!(!path.join(".git").exists(), "bare: no .git directory");
    assert_eq!(
        std::fs::read_to_string(path.join("HEAD")).unwrap().trim(),
        "ref: refs/heads/main"
    );
    let trunk = r.init(&slug("core"), &slug("db"), "trunk").await.unwrap();
    assert_eq!(
        std::fs::read_to_string(trunk.join("HEAD")).unwrap().trim(),
        "ref: refs/heads/trunk"
    );
}

#[tokio::test]
async fn init_refuses_an_existing_directory() {
    let dir = tempfile::tempdir().unwrap();
    let r = repos(dir.path());
    r.init(&slug("core"), &slug("api"), "main").await.unwrap();
    let err = r
        .init(&slug("core"), &slug("api"), "main")
        .await
        .expect_err("second init fails");
    assert!(matches!(err, RepoError::Exists { .. }), "got {err:?}");
}

#[tokio::test]
async fn init_reports_a_failing_git() {
    let dir = tempfile::tempdir().unwrap();
    let r = Repos::new(dir.path().to_path_buf(), dir.path().join("no-such-git"));
    let err = r
        .init(&slug("core"), &slug("api"), "main")
        .await
        .expect_err("spawn fails");
    assert!(
        matches!(err, RepoError::Io { .. }),
        "a missing binary is an io error: {err:?}"
    );
    assert!(
        !dir.path().join("core").join("api.git").exists(),
        "a failed spawn releases the claim"
    );
    let r = repos(dir.path());
    let err = r
        .init(&slug("core"), &slug("bad"), "not a branch")
        .await
        .expect_err("git refuses the branch name");
    assert!(matches!(err, RepoError::GitInit { .. }), "got {err:?}");
    assert!(
        !dir.path()
            .join("core")
            .join("bad.git")
            .join("HEAD")
            .exists(),
        "nothing half-made is left"
    );
}

#[tokio::test]
async fn two_concurrent_inits_of_one_repository_claim_it_once() {
    let dir = tempfile::tempdir().unwrap();
    let r = repos(dir.path());
    let (team, ws) = (slug("core"), slug("api"));
    let (a, b) = tokio::join!(r.init(&team, &ws, "main"), r.init(&team, &ws, "main"));
    let (won, lost) = match (a, b) {
        (Ok(p), Err(e)) | (Err(e), Ok(p)) => (p, e),
        other => panic!("exactly one init wins: {other:?}"),
    };
    assert_eq!(won, dir.path().join("core").join("api.git"));
    assert!(matches!(lost, RepoError::Exists { .. }), "got {lost:?}");
    assert_eq!(
        std::fs::read_to_string(won.join("HEAD")).unwrap().trim(),
        "ref: refs/heads/main",
        "the winner's repository is whole"
    );
}

#[tokio::test]
async fn remove_deletes_the_repository_and_reports_absence() {
    let dir = tempfile::tempdir().unwrap();
    let r = repos(dir.path());
    let path = r.init(&slug("core"), &slug("api"), "main").await.unwrap();
    assert!(
        r.remove(&slug("core"), &slug("api")).await.unwrap(),
        "removed"
    );
    assert!(!path.exists());
    assert!(
        !r.remove(&slug("core"), &slug("api")).await.unwrap(),
        "already gone"
    );
    r.init(&slug("core"), &slug("api"), "main").await.unwrap();
    assert!(path.join("HEAD").is_file(), "the slug is free again");
}

const SEED_COLLECTION: &[u8] = b"opencollection: 1.0.0\n\ninfo:\n  name: Workspace\nbundled: false\nextensions:\n  bruno:\n    ignore:\n      - node_modules\n      - .git\n";

fn apinata_seed() -> [(&'static str, &'static [u8]); 2] {
    [
        (".gitignore", b".env\n"),
        ("opencollection.yml", SEED_COLLECTION),
    ]
}

/// `git --git-dir=<repo> <args>` through the isolated runner; panics with stderr on failure.
fn in_repo(home: &Path, repo: &Path, args: &[&str]) -> String {
    let mut full = vec!["--git-dir", repo.to_str().unwrap()];
    full.extend_from_slice(args);
    ok(git(home, None, None, &full))
}

#[tokio::test]
async fn seed_commit_is_the_same_sha_on_two_repositories() {
    let dir = tempfile::tempdir().unwrap();
    let r = repos(dir.path());
    let mut shas = Vec::new();
    for ws in ["one", "two"] {
        let repo = r.init(&slug("core"), &slug(ws), "main").await.unwrap();
        let sha = r
            .seed_commit(
                &repo,
                "main",
                &apinata_seed(),
                "Potluck",
                "potluck@users.noreply.127.0.0.1",
                "Seed apinata workspace",
            )
            .await
            .unwrap();
        assert_eq!(
            in_repo(
                dir.path(),
                &repo,
                &["rev-parse", "--verify", "refs/heads/main"]
            )
            .trim(),
            sha
        );
        shas.push(sha);
    }
    assert_eq!(
        shas[0], shas[1],
        "same files, identity, message and date: same commit"
    );
    assert_eq!(
        shas[0], SEED_COMMIT,
        "measured on git 2.56.1; a change here changes what every server seeds"
    );
    let repo = r.path(&slug("core"), &slug("one"));
    assert_eq!(
        in_repo(dir.path(), &repo, &["ls-tree", "-r", "refs/heads/main"]),
        "100644 blob 4c49bd78f1d08f2bc09fa0bd8191ed38b7dce5e3\t.gitignore\n100644 blob f232e666589f142205abdb96ad168c970b218c9e\topencollection.yml\n"
    );
    assert_eq!(
        in_repo(
            dir.path(),
            &repo,
            &[
                "log",
                "--date=iso-strict",
                "--format=%an <%ae> %ad|%cn <%ce> %cd|%P|%s",
                "refs/heads/main"
            ]
        ),
        "Potluck <potluck@users.noreply.127.0.0.1> 2026-01-01T00:00:00Z|Potluck <potluck@users.noreply.127.0.0.1> 2026-01-01T00:00:00Z||Seed apinata workspace\n",
        "one root commit, the Potluck identity, the fixed date"
    );
    assert_eq!(
        in_repo(
            dir.path(),
            &repo,
            &[
                "cat-file",
                "blob",
                "f232e666589f142205abdb96ad168c970b218c9e"
            ]
        )
        .as_bytes(),
        SEED_COLLECTION
    );
    let other = r
        .seed_commit(
            &r.init(&slug("core"), &slug("three"), "main").await.unwrap(),
            "main",
            &apinata_seed(),
            "Potluck",
            "potluck@users.noreply.team.example.com",
            "Seed apinata workspace",
        )
        .await
        .unwrap();
    assert_ne!(
        other, SEED_COMMIT,
        "another host is another identity, so another commit"
    );
}

#[tokio::test]
async fn seed_commit_refuses_nested_paths_and_an_existing_branch() {
    let dir = tempfile::tempdir().unwrap();
    let r = repos(dir.path());
    let repo = r.init(&slug("core"), &slug("api"), "main").await.unwrap();
    for bad in ["sub/x", "", "/x", ".", "..", "a\nb", "a\tb"] {
        let err = r
            .seed_commit(&repo, "main", &[(bad, b"1")], "P", "p@h", "m")
            .await
            .unwrap_err();
        assert!(
            matches!(err, RepoError::SeedPath { .. }),
            "{bad:?}: got {err:?}"
        );
    }
    assert_eq!(
        in_repo(dir.path(), &repo, &["for-each-ref"]),
        "",
        "nothing was written"
    );
    r.seed_commit(&repo, "main", &apinata_seed(), "P", "p@h", "m")
        .await
        .unwrap();
    let err = r
        .seed_commit(&repo, "main", &[("other", b"2")], "P", "p@h", "m")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RepoError::Seed {
                step: "update-ref",
                ..
            }
        ),
        "an existing branch is never overwritten: {err:?}"
    );
    assert_eq!(
        in_repo(
            dir.path(),
            &repo,
            &["ls-tree", "--name-only", "refs/heads/main"]
        ),
        ".gitignore\nopencollection.yml\n"
    );
    let err = r
        .seed_commit(
            &dir.path().join("nope.git"),
            "main",
            &apinata_seed(),
            "P",
            "p@h",
            "m",
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RepoError::Seed {
                step: "hash-object",
                ..
            }
        ),
        "a missing repository fails at the first step: {err:?}"
    );
}

#[tokio::test]
async fn seed_commit_refuses_bad_and_duplicate_names_before_spawning_git() {
    let dir = tempfile::tempdir().unwrap();
    // No git binary at all: only a refusal ahead of every spawn can produce `SeedPath`.
    let r = Repos::new(dir.path().to_path_buf(), dir.path().join("no-such-git"));
    let repo = dir.path().join("core").join("api.git");
    let cases: [&[(&str, &[u8])]; 2] = [
        &[("ok", b"1"), ("sub/x", b"2")],
        &[("same", b"1"), ("same", b"2")],
    ];
    for files in cases {
        let err = r
            .seed_commit(&repo, "main", files, "P", "p@h", "m")
            .await
            .unwrap_err();
        assert!(
            matches!(err, RepoError::SeedPath { .. }),
            "{files:?}: got {err:?}"
        );
    }
}

/// Whether the process `<dir>/hung.pid` names is gone; a killed but unreaped one would remain.
fn hung_git_is_gone(dir: &Path) -> bool {
    let pid = std::fs::read_to_string(dir.join("hung.pid")).expect("the wrapper hung");
    !Path::new(&format!("/proc/{}", pid.trim())).exists()
}

#[tokio::test]
async fn init_kills_a_git_that_runs_past_the_limit_and_frees_the_slug() {
    let dir = tempfile::tempdir().unwrap();
    let wrapper = hanging_git(dir.path(), "init");
    let r = Repos::new(dir.path().join("repos"), wrapper)
        .with_max_runtime(std::time::Duration::from_millis(500));
    let began = std::time::Instant::now();
    let err = r
        .init(&slug("acme"), &slug("ws"), "main")
        .await
        .expect_err("a hung init fails");
    assert!(
        matches!(err, RepoError::Overran { step: "init", .. }),
        "got {err:?}"
    );
    assert!(
        began.elapsed() < std::time::Duration::from_secs(10),
        "took {:?}",
        began.elapsed()
    );
    assert!(
        !r.path(&slug("acme"), &slug("ws")).exists(),
        "the claim is removed"
    );
    assert!(
        hung_git_is_gone(dir.path()),
        "the hung git is killed and reaped"
    );
}

#[tokio::test]
async fn seed_commit_kills_a_plumbing_git_that_runs_past_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repos(&dir.path().join("repos"))
        .init(&slug("acme"), &slug("ws"), "main")
        .await
        .unwrap();
    let wrapper = hanging_git(dir.path(), "hash-object");
    let r = Repos::new(dir.path().join("repos"), wrapper)
        .with_max_runtime(std::time::Duration::from_millis(500));
    let began = std::time::Instant::now();
    let err = r
        .seed_commit(&repo, "main", &apinata_seed(), "P", "p@h", "m")
        .await
        .expect_err("a hung hash-object fails");
    assert!(
        matches!(
            err,
            RepoError::Overran {
                step: "hash-object",
                ..
            }
        ),
        "got {err:?}"
    );
    assert!(
        began.elapsed() < std::time::Duration::from_secs(10),
        "took {:?}",
        began.elapsed()
    );
    assert!(
        hung_git_is_gone(dir.path()),
        "the hung git is killed and reaped"
    );
}
