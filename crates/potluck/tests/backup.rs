//! `potluck::backup::write_backup` against a live server's data directory.

use std::fs::File;
use std::path::{Path, PathBuf};

use potluck::backup::{BackupError, BackupOptions, Phase, Written, write_backup};

use crate::common::{
    FullServer, api_admin, api_team, api_workspace, config_for, full_server, git, ok, repo_path,
    work_tree,
};

fn options() -> BackupOptions {
    BackupOptions::default().with_reserve_bytes(0)
}

/// Runs the backup on a blocking thread, so `on_phase` may drive the server with
/// `Handle::block_on`.
pub(crate) async fn backup_with<F>(
    data: PathBuf,
    options: BackupOptions,
    on_phase: F,
) -> Result<(Written, Vec<u8>), BackupError>
where
    F: FnMut(Phase) + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut on_phase = on_phase;
        let mut out = Vec::new();
        write_backup(&data, Path::new("git"), &options, &mut out, &mut on_phase).map(|w| (w, out))
    })
    .await
    .expect("backup thread")
}

/// Unpacks `tar` into `into`; returns each entry's path, uid and gid in order.
pub(crate) fn unpack(tar: &[u8], into: &Path) -> Vec<(String, u64, u64)> {
    let mut archive = tar::Archive::new(tar);
    let mut seen = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        let header = entry.header();
        seen.push((path, header.uid().unwrap(), header.gid().unwrap()));
        entry.unpack_in(into).unwrap();
    }
    seen
}

pub(crate) fn refs(repo: &Path) -> String {
    let out = std::process::Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("--git-dir")
        .arg(repo)
        .args(["for-each-ref", "--format=%(objectname) %(refname)"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A server with team `core`, seeded `seeded`, unseeded `pushed` (one push of `main` as
/// root) and unseeded `fresh`; returns the server, root's token and a git home.
pub(crate) async fn three_workspaces() -> (FullServer, String, PathBuf) {
    let server = full_server().await;
    let root = api_admin(server.db()).await;
    api_team(&server.api(""), &root, "core").await;
    api_workspace(&server.api(""), &root, "core", "seeded", true).await;
    api_workspace(&server.api(""), &root, "core", "pushed", false).await;
    api_workspace(&server.api(""), &root, "core", "fresh", false).await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = work_tree(&home, "work");
    ok(git(
        &home,
        Some(&work),
        Some(&root),
        &["push", "-q", &server.git_url("core", "pushed"), "main"],
    ));
    (server, root, home)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_backup_restores_into_an_empty_directory_and_every_workspace_clones() {
    let (server, root, home) = three_workspaces().await;
    let data = server.dir.path().to_path_buf();
    let (written, tar) = backup_with(data.clone(), options(), |_| {}).await.unwrap();
    assert_eq!(written.workspaces, 3);
    let restored = tempfile::tempdir().unwrap();
    let entries = unpack(&tar, restored.path());
    let names: Vec<&str> = entries.iter().map(|(p, _, _)| p.as_str()).collect();
    assert_eq!(names[0], "server.db", "{names:?}");
    assert!(
        names[1..].iter().all(|n| n.starts_with("repos/")),
        "only repos follow: {names:?}"
    );
    assert!(names.contains(&"repos/core/fresh.git/HEAD"), "{names:?}");
    assert!(
        entries
            .iter()
            .all(|(_, uid, gid)| (*uid, *gid) == (65532, 65532)),
        "owners"
    );
    assert!(!restored.path().join("server.db-wal").exists());
    for ws in ["seeded", "pushed", "fresh"] {
        assert_eq!(
            refs(&repo_path(restored.path(), "core", ws)),
            refs(&repo_path(&data, "core", ws)),
            "{ws}"
        );
    }
    assert!(!data.join("tmp/backup").exists(), "staging removed");
    let running = potluck::server::start(&config_for(restored.path()))
        .await
        .expect("restored server starts");
    for ws in ["seeded", "pushed"] {
        let url = format!("http://{}/git/core/{ws}.git", running.http_addr());
        ok(git(
            &home,
            Some(&home),
            Some(&root),
            &["clone", "-q", &url, &format!("clone-{ws}")],
        ));
    }
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_staging_directory_is_cleared_first() {
    let (server, _, _) = three_workspaces().await;
    let data = server.dir.path().to_path_buf();
    std::fs::create_dir_all(data.join("tmp/backup/repos/core/stale.git")).unwrap();
    let (_, tar) = backup_with(data.clone(), options(), |_| {}).await.unwrap();
    let restored = tempfile::tempdir().unwrap();
    let names: Vec<String> = unpack(&tar, restored.path())
        .into_iter()
        .map(|(p, _, _)| p)
        .collect();
    assert!(!names.iter().any(|n| n.contains("stale")), "{names:?}");
    assert!(!data.join("tmp/backup").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn refusals_come_before_any_copy() {
    let empty = tempfile::tempdir().unwrap();
    let err = backup_with(empty.path().to_path_buf(), options(), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(err, BackupError::NoDatabase { .. }), "{err}");

    let (server, _, _) = three_workspaces().await;
    let data = server.dir.path().to_path_buf();
    let lock = File::create(data.join("tmp/backup.lock")).unwrap();
    lock.try_lock().unwrap();
    let err = backup_with(data.clone(), options(), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(err, BackupError::InProgress), "{err}");
    drop(lock);

    let huge = BackupOptions::default().with_reserve_bytes(u64::MAX / 2);
    let err = backup_with(data.clone(), huge, |_| {}).await.unwrap_err();
    assert!(matches!(err, BackupError::NoSpace { .. }), "{err}");
    assert!(!data.join("tmp/backup").exists(), "nothing staged");
}
