//! `Repos` creates and locates bare repositories under a temp root.

use std::path::{Path, PathBuf};

use potluck::git::{RepoError, Repos};
use potluck::slug::Slug;

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
