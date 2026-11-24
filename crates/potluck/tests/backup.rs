//! `potluck::backup::write_backup` against a live server's data directory.

use std::fs::File;
use std::path::{Path, PathBuf};

use potluck::backup::{BackupError, BackupOptions, Phase, Written, write_backup};
use reqwest::Method;
use serde_json::json;

use crate::common::{
    FullServer, api_admin, api_team, api_user, api_workspace, call, config_for, full_server, git,
    ok, repo_path, work_tree,
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
    // A sibling test's git child, forked but not yet exec'd, holds a copy of this fd; only an
    // explicit unlock releases the lock regardless of such copies.
    lock.unlock().unwrap();
    drop(lock);

    let huge = BackupOptions::default().with_reserve_bytes(u64::MAX / 2);
    let err = backup_with(data.clone(), huge, |_| {}).await.unwrap_err();
    assert!(matches!(err, BackupError::NoSpace { .. }), "{err}");
    assert!(!data.join("tmp/backup").exists(), "nothing staged");
}

fn backup_db(tar: &[u8]) -> (tempfile::TempDir, rusqlite::Connection) {
    let dir = tempfile::tempdir().unwrap();
    unpack(tar, dir.path());
    let conn = rusqlite::Connection::open(dir.path().join("server.db")).unwrap();
    (dir, conn)
}

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn delete_ws(api: &str, token: &str, ws: &str) {
    let rt = tokio::runtime::Handle::current();
    let (status, body) = rt.block_on(call(
        &format!("{api}/workspaces/core/{ws}"),
        Method::DELETE,
        Some(token),
        None,
    ));
    assert_eq!(status, 204, "delete {ws}: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn workspaces_created_deleted_or_recreated_mid_backup_are_absent() {
    let (server, root, _) = three_workspaces().await;
    let api = server.api("");
    let rt = tokio::runtime::Handle::current();
    let (r, a) = (root.clone(), api.clone());
    let (written, tar) = backup_with(server.dir.path().to_path_buf(), options(), move |phase| {
        if let Phase::ReposCopied = phase {
            rt.block_on(api_workspace(&a, &r, "core", "late", true));
            delete_ws(&a, &r, "fresh");
            delete_ws(&a, &r, "seeded");
            rt.block_on(api_workspace(&a, &r, "core", "seeded", false));
        }
    })
    .await
    .unwrap();
    let (dir, conn) = backup_db(&tar);
    let rows: Vec<String> = conn
        .prepare("SELECT slug FROM workspaces ORDER BY slug")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows, ["pushed"], "only the untouched workspace");
    for ws in ["late", "fresh", "seeded"] {
        assert!(
            !repo_path(dir.path(), "core", ws).exists(),
            "{ws} repository absent"
        );
    }
    assert_eq!(written.workspaces, 1);
    assert_eq!(
        written.dropped_rows, 2,
        "late (no copy) and recreated seeded"
    );
    assert_eq!(
        written.dropped_repos, 2,
        "fresh (no row) and the old seeded copy"
    );
    let lines: Vec<String> = written.left_out.iter().map(ToString::to_string).collect();
    assert_eq!(
        lines,
        [
            "left out: core/fresh (deleted during the backup)",
            "left out: core/late (created during the backup)",
            "left out: core/seeded (deleted during the backup)",
        ],
        "one line per left-out workspace"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM grants g LEFT JOIN workspaces w ON w.id = g.workspace_id WHERE w.id IS NULL"
        ),
        0,
        "no dangling grants"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_workspace_whose_branch_is_deleted_and_pruned_mid_backup_stays() {
    let (server, root, home) = three_workspaces().await;
    let url = server.git_url("core", "pushed");
    let work = home.join("work");
    // `topic` carries a commit of its own, so deleting it leaves objects for the prune to remove.
    ok(git(
        &home,
        Some(&work),
        None,
        &["checkout", "-q", "-b", "topic"],
    ));
    std::fs::write(work.join("topic.txt"), "only on topic\n").unwrap();
    ok(git(&home, Some(&work), None, &["add", "topic.txt"]));
    ok(git(
        &home,
        Some(&work),
        None,
        &["commit", "-q", "-m", "topic"],
    ));
    ok(git(
        &home,
        Some(&work),
        Some(&root),
        &["push", "-q", &url, "topic"],
    ));
    ok(git(&home, Some(&work), None, &["checkout", "-q", "main"]));
    let src = repo_path(server.dir.path(), "core", "pushed");
    let (h, w, u, r) = (home.clone(), work.clone(), url.clone(), root.clone());
    let (written, tar) = backup_with(server.dir.path().to_path_buf(), options(), move |phase| {
        if let Phase::ReposCopied = phase {
            ok(git(
                &h,
                Some(&w),
                Some(&r),
                &["push", "-q", &u, ":refs/heads/topic"],
            ));
            ok(git(
                &h,
                Some(&h),
                None,
                &[
                    "--git-dir",
                    src.to_str().unwrap(),
                    "gc",
                    "-q",
                    "--prune=now",
                ],
            ));
        }
    })
    .await
    .unwrap();
    let (_dir, conn) = backup_db(&tar);
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM workspaces WHERE slug = 'pushed'"
        ),
        1,
        "still backed up"
    );
    assert_eq!(written.dropped_rows, 0);
    assert!(
        written.left_out.is_empty(),
        "nothing left out: {:?}",
        written.left_out
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_backup_never_carries_an_armed_override_or_a_stale_import_flag() {
    let (server, root, _) = three_workspaces().await;
    let (_, _bob) = api_user(server.db(), "bob").await;
    let (status, body) = call(
        &server.api("/workspaces/core/seeded/overrides"),
        Method::POST,
        Some(&root),
        Some(json!({"for_user": "bob", "reason": "pending import"})),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    // The push to `pushed` cleared its flag; set it again as if its bookkeeping had not landed.
    let live = rusqlite::Connection::open(server.dir.path().join("server.db")).unwrap();
    live.busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    live.execute(
        "UPDATE workspaces SET import_pending = 1 WHERE slug = 'pushed'",
        [],
    )
    .unwrap();
    let (written, tar) = backup_with(server.dir.path().to_path_buf(), options(), |_| {})
        .await
        .unwrap();
    let (_dir, conn) = backup_db(&tar);
    assert_eq!(written.disarmed, 1);
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM overrides WHERE consumed_at IS NULL"
        ),
        0,
        "no armed override"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT import_pending FROM workspaces WHERE slug = 'pushed'"
        ),
        0,
        "has refs: cleared"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT import_pending FROM workspaces WHERE slug = 'fresh'"
        ),
        1,
        "empty: still pending"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_held_in_pre_receive_is_not_in_the_backup_and_then_lands() {
    let (server, root, home) = three_workspaces().await;
    let marks = server.dir.path().join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    let m = marks.display();
    crate::common::write_hook(
        &server.hooks_dir(),
        &format!("touch '{m}/started'\nwhile [ ! -e '{m}/release' ]; do sleep 0.05; done\nexit 0"),
    );
    let src = repo_path(server.dir.path(), "core", "pushed");
    let before = refs(&src);
    let work = home.join("work");
    std::fs::write(work.join("README"), "two\n").unwrap();
    ok(git(
        &home,
        Some(&work),
        None,
        &["commit", "-q", "-am", "two"],
    ));
    let (h, w, u, r) = (
        home.clone(),
        work.clone(),
        server.git_url("core", "pushed"),
        root.clone(),
    );
    let push = std::thread::spawn(move || git(&h, Some(&w), Some(&r), &["push", "-q", &u, "main"]));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !marks.join("started").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the hook never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let (_, tar) = backup_with(server.dir.path().to_path_buf(), options(), |_| {})
        .await
        .unwrap();
    let restored = tempfile::tempdir().unwrap();
    unpack(&tar, restored.path());
    assert_eq!(
        refs(&repo_path(restored.path(), "core", "pushed")),
        before,
        "the held push is not in the backup"
    );
    std::fs::write(marks.join("release"), b"").unwrap();
    ok(push.join().unwrap());
    assert_ne!(refs(&src), before, "the push landed after the backup");
}
