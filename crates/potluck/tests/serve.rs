//! `server::start` end to end on a temp data dir: TCP, the admin socket, the data layout,
//! shutdown, and the `serve` subcommand's refusals through the binary.

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use potluck::api::AppState;
use potluck::config::ConfigError;
use potluck::db::Db;
use potluck::role::Role;
use potluck::server::{ServerError, start, start_with};
use serde_json::Value;

use crate::common::{
    TestAccess, api_admin, api_team, api_workspace, config_for, config_listening_on, create_user,
    git, ok, parse_config, potluck, repo_path, unix_get, work_tree, write_hook, write_script,
};

#[tokio::test]
async fn start_serves_health_on_loopback_and_prepares_the_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let running = start(&config_for(dir.path())).await.unwrap();
    assert_eq!(running.http_addr().ip().to_string(), "127.0.0.1");
    let body: Value = reqwest::get(format!("http://{}/api/v1/health", running.http_addr()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "ok");
    assert!(dir.path().join("server.db").is_file(), "server.db");
    assert!(dir.path().join("repos").is_dir(), "repos/");
    assert!(dir.path().join("hooks").is_dir(), "hooks/");
    assert!(dir.path().join("server.lock").is_file(), "server.lock");
    let version: i64 = rusqlite::Connection::open(dir.path().join("server.db"))
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 4, "migrations ran at start");
    running.shutdown().await;
}

#[tokio::test]
async fn start_with_serves_the_given_state_behind_the_same_lock_and_sockets() {
    const READER: &str = "plk_reader_token_000000000000000000000000000";
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(dir.path());
    let db = Db::open(&config.db_path()).unwrap();
    let access =
        TestAccess::new()
            .workspace("core", "api")
            .grant(READER, "core", "api", Role::Read);
    let state = AppState::new(db, config.clone()).with_git_access(Arc::new(access));
    let running = start_with(&config, state).await.unwrap();
    // Only the given resolver knows this token; it may read, so a push is refused with 403.
    let r = reqwest::Client::new()
        .get(format!(
            "http://{}/git/core/api.git/info/refs?service=git-receive-pack",
            running.http_addr()
        ))
        .bearer_auth(READER)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403, "the given resolver answered");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "forbidden");
    // The state's own database was migrated: the users table exists.
    assert!(
        running
            .db()
            .user_by_username("nobody")
            .await
            .unwrap()
            .is_none()
    );
    let second = start(&config).await;
    assert!(
        matches!(second, Err(ServerError::DataDirInUse { .. })),
        "the data dir is locked: {second:?}"
    );
    let (status, _) = unix_get(running.admin_socket(), "/api/v1/health").await;
    assert_eq!(status, 200, "the admin socket serves too");
    running.shutdown().await;
}

#[tokio::test]
async fn start_creates_a_missing_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("nested").join("data");
    let running = start(&config_for(&data)).await.unwrap();
    assert!(data.join("server.db").is_file());
    running.shutdown().await;
}

#[tokio::test]
async fn start_reports_an_unwritable_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let err = start(&config_for(&blocker.join("data")))
        .await
        .expect_err("a file cannot be a parent");
    assert!(matches!(err, ServerError::DataDir { .. }), "got {err:?}");
}

#[tokio::test]
async fn the_admin_socket_serves_the_api_and_shutdown_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("admin.sock"), b"stale").unwrap();
    let running = start(&config_for(dir.path())).await.unwrap();
    let socket = running.admin_socket().to_path_buf();
    let (status, body) = unix_get(&socket, "/api/v1/health").await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["status"],
        "ok"
    );
    let (status, body) = unix_get(&socket, "/api/v1/nope").await;
    assert_eq!(status, 404);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
        "not_found"
    );
    running.shutdown().await;
    assert!(!socket.exists(), "shutdown removes the socket file");
}

#[tokio::test]
async fn shutdown_closes_the_tcp_listener_and_a_restart_reuses_the_data() {
    let dir = tempfile::tempdir().unwrap();
    // A bound, never-listening SO_REUSEADDR socket reserves the port for this test: the kernel
    // hands it to no other bind(0) while the guard lives, yet our own listener may share it.
    let guard = tokio::net::TcpSocket::new_v4().unwrap();
    guard.set_reuseaddr(true).unwrap();
    guard.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = guard.local_addr().unwrap();
    let config = config_listening_on(dir.path(), addr);
    let running = start(&config).await.unwrap();
    assert_eq!(
        running.http_addr(),
        addr,
        "the server took the reserved port"
    );
    create_user(running.db(), "alice", "correct horse battery", false).await;
    running.shutdown().await;
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err(),
        "the port is closed"
    );
    let again = start(&config).await.unwrap();
    assert_eq!(again.http_addr(), addr, "the restart binds the same port");
    let body: Value = reqwest::get(format!("http://{addr}/api/v1/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "ok");
    let alice = again.db().user_by_username("alice").await.unwrap();
    assert!(alice.is_some(), "the restart serves the first run's data");
    again.shutdown().await;
    drop(guard);
}

#[test]
fn serve_without_public_url_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = potluck()
        .args(["serve", "--data"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--public-url"));
}

#[test]
fn serve_refuses_a_non_loopback_http_public_url_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let out = potluck()
        .env("POTLUCK_PUBLIC_URL", "http://team.example.com")
        .args(["serve", "--data"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--insecure-http"), "{stderr}");
    assert!(stderr.contains("team.example.com"), "{stderr}");
    assert!(
        !dir.path().join("server.db").exists(),
        "nothing is created before config passes"
    );
}

#[test]
fn serve_on_a_busy_port_prints_the_cause_once() {
    let dir = tempfile::tempdir().unwrap();
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = busy.local_addr().unwrap().to_string();
    let out = potluck()
        .args([
            "serve",
            "--listen",
            &addr,
            "--public-url",
            "http://127.0.0.1:8080",
            "--data",
        ])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "a bind failure exits 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("Address already in use").count(),
        1,
        "{stderr}"
    );
}

#[test]
fn serve_refuses_a_partial_oidc_set_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let child = potluck()
        .env("POTLUCK_PUBLIC_URL", "http://127.0.0.1:8080")
        .env("POTLUCK_OIDC_CLIENT_ID", "potluck")
        .env("POTLUCK_LISTEN", "127.0.0.1:0")
        .args(["serve", "--data"])
        .arg(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (exited, out) = output_within(child, Duration::from_secs(5));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(exited, "serve started despite the OIDC flags: {stderr}");
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("OIDC"), "{stderr}");
    assert!(
        !dir.path().join("server.db").exists(),
        "nothing is created before config passes"
    );
}

#[test]
fn serve_with_oidc_starts_while_the_idp_is_down_and_logs_the_redirect_uri() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("oidc-secret");
    std::fs::write(&secret, "s3cret-value\n").unwrap();
    let dead = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut child = potluck()
        .env("POTLUCK_PUBLIC_URL", "http://127.0.0.1:8080")
        .env("POTLUCK_LISTEN", "127.0.0.1:0")
        .env("POTLUCK_OIDC_ISSUER", format!("http://{dead}"))
        .env("POTLUCK_OIDC_CLIENT_ID", "potluck")
        .env("POTLUCK_OIDC_CLIENT_SECRET_FILE", &secret)
        .args(["serve", "--data"])
        .arg(dir.path().join("data"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let still_running = child.try_wait().unwrap().is_none();
    child.kill().unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(still_running, "serve exited: {stderr}");
    assert!(
        stderr.contains("http://127.0.0.1:8080/api/v1/oidc/callback"),
        "{stderr}"
    );
    assert!(!stderr.contains("s3cret-value"), "{stderr}");
    let probe_warns = stderr
        .lines()
        .filter(|line| line.contains("WARN") && line.contains("provider"))
        .count();
    assert_eq!(probe_warns, 1, "one warning per failed probe: {stderr}");
}

/// Waits up to `limit` for `child` to exit, killing and reaping it if it has not, so a serve
/// that should have refused to start fails its test instead of hanging the suite.
fn output_within(mut child: std::process::Child, limit: Duration) -> (bool, std::process::Output) {
    let deadline = Instant::now() + limit;
    let exited = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !exited {
        let _ = child.kill();
    }
    (exited, child.wait_with_output().unwrap())
}

#[tokio::test]
async fn start_refuses_a_data_dir_another_server_holds() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(dir.path());
    let first = start(&config).await.unwrap();
    let began = Instant::now();
    let err = start(&config).await.expect_err("the data dir is held");
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "the refusal is immediate, took {:?}",
        began.elapsed()
    );
    match &err {
        ServerError::DataDirInUse { path } => assert_eq!(path, config.data()),
        other => panic!("got {other:?}"),
    }
    assert!(err.to_string().contains("in use"), "{err}");
    let body: Value = reqwest::get(format!("http://{}/api/v1/health", first.http_addr()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "ok", "the first server keeps serving");
    let (status, _) = unix_get(first.admin_socket(), "/api/v1/health").await;
    assert_eq!(status, 200, "the first server's admin socket is untouched");
    first.shutdown().await;
}

#[tokio::test]
async fn a_stale_lock_file_does_not_block_a_start_and_shutdown_releases_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("server.lock"), b"left by a crash").unwrap();
    let config = config_for(dir.path());
    let running = start(&config).await.unwrap();
    running.shutdown().await;
    assert!(
        dir.path().join("server.lock").is_file(),
        "shutdown leaves the file; the kernel released the lock"
    );
    let again = start(&config).await.unwrap();
    again.shutdown().await;
}

#[test]
fn serve_exits_when_another_process_holds_the_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let held = std::fs::File::create(dir.path().join("server.lock")).unwrap();
    held.try_lock().unwrap();
    let child = potluck()
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--public-url",
            "http://127.0.0.1:8080",
            "--data",
        ])
        .arg(dir.path())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (exited, out) = output_within(child, Duration::from_secs(5));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(exited, "serve kept running on a held data dir: {stderr}");
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("in use"), "{stderr}");
    assert!(stderr.contains(dir.path().to_str().unwrap()), "{stderr}");
    assert!(
        !dir.path().join("server.db").exists(),
        "the database is not opened before the lock"
    );
    assert!(
        !dir.path().join("hooks").join("pre-receive").exists(),
        "the hook is not written before the lock"
    );
    drop(held);
}

/// `base` plus one padding directory, so that `<result>/admin.sock` is exactly `len` bytes.
fn data_dir_with_socket_len(base: &std::path::Path, len: usize) -> std::path::PathBuf {
    let fixed = base.as_os_str().len() + "/".len() + "/admin.sock".len();
    assert!(
        fixed < len,
        "temp dir {} is too long for this test",
        base.display()
    );
    base.join("d".repeat(len - fixed))
}

#[tokio::test]
async fn start_binds_the_admin_socket_at_the_longest_accepted_path() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir_with_socket_len(dir.path(), 103);
    let running = start(&config_for(&data)).await.unwrap();
    assert_eq!(running.admin_socket().as_os_str().len(), 103);
    let (status, _) = unix_get(running.admin_socket(), "/api/v1/health").await;
    assert_eq!(status, 200);
    running.shutdown().await;
}

#[test]
fn serve_refuses_a_data_dir_too_long_for_the_admin_socket_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir_with_socket_len(dir.path(), 104);
    let out = potluck()
        .args(["serve", "--public-url", "http://127.0.0.1:8080", "--data"])
        .arg(&data)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains(&*data.join("admin.sock").to_string_lossy()),
        "{stderr}"
    );
    assert!(stderr.contains("104"), "{stderr}");
    assert!(stderr.contains("103"), "{stderr}");
    assert!(!stderr.contains("SUN_LEN"), "{stderr}");
    assert!(!data.exists(), "nothing is created before config passes");
}

// `serve` checks the socket path itself; a library caller of `start` gets the same refusal,
// before any directory exists, instead of a bind error after creating the layout.
#[tokio::test]
async fn start_refuses_a_data_dir_too_long_for_the_admin_socket_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir_with_socket_len(dir.path(), 104);
    let err = start(&config_for(&data))
        .await
        .expect_err("an over-long socket path is refused");
    assert!(
        matches!(
            err,
            ServerError::Config(ConfigError::AdminSocketPathTooLong {
                len: 104,
                max: 103,
                ..
            })
        ),
        "got {err:?}"
    );
    let text = err.to_string();
    assert!(text.contains("104") && text.contains("103"), "{text}");
    assert!(!data.exists(), "nothing is created before the check");
}

// A receive-pack killed while no server was there to sweep (a crash, the OOM killer, a reboot)
// leaves its debris for the next start to clear before anything can push.
#[tokio::test(flavor = "multi_thread")]
async fn a_start_sweeps_killed_push_leftovers_and_the_push_then_lands() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(dir.path());
    let first = start(&config).await.unwrap();
    let root = api_admin(first.db()).await;
    let api = format!("http://{}/api/v1", first.http_addr());
    api_team(&api, &root, "core").await;
    api_workspace(&api, &root, "core", "api", false).await;
    first.shutdown().await;

    let repo = repo_path(dir.path(), "core", "api");
    let put = |rel: &str, text: &str| {
        let path = repo.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    let debris = [
        "objects/tmp_objdir-incoming-AbC123/pack/tmp_pack_xyz",
        "refs/heads/main.lock",
        "HEAD.lock",
        "packed-refs.lock",
    ];
    for rel in debris {
        put(rel, "x");
    }
    put(
        "objects/pack/pack-aaaa.keep",
        "receive-pack 4018551 on host\n",
    );
    put("objects/pack/pack-bbbb.keep", "kept by the operator\n");

    let again = start(&config).await.unwrap();
    // The installed hook execs the serving binary, which here is the test binary.
    write_hook(&dir.path().join("hooks"), "exit 0");
    assert!(
        !repo.join("objects/tmp_objdir-incoming-AbC123").exists(),
        "the quarantine is swept"
    );
    for rel in &debris[1..] {
        assert!(!repo.join(rel).exists(), "{rel} is swept");
    }
    assert!(
        !repo.join("objects/pack/pack-aaaa.keep").exists(),
        "receive-pack's keep is swept"
    );
    assert!(
        repo.join("objects/pack/pack-bbbb.keep").exists(),
        "an operator keep stays"
    );
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = work_tree(&home, "work");
    let url = format!("http://{}/git/core/api.git", again.http_addr());
    ok(git(
        &home,
        Some(&work),
        Some(&root),
        &["push", "-q", &url, "main"],
    ));
    again.shutdown().await;
}

/// A `git hash-object --stdin` in its own process group holding `path` open, as a detached
/// maintenance run holds its lock from outside git's group. Killed and reaped on drop.
struct LockHolder(std::process::Child);

impl LockHolder {
    fn open(path: &std::path::Path) -> LockHolder {
        use std::os::unix::process::CommandExt;
        let lock = std::fs::File::options().append(true).open(path).unwrap();
        let child = std::process::Command::new("git")
            .args(["hash-object", "--stdin"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::piped())
            .stdout(lock)
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let holder = LockHolder(child);
        // The sweep checks the holder's argv, so wait until /proc shows git; the holder is
        // already built, so a timeout still kills and reaps it.
        let cmdline = format!("/proc/{}/cmdline", holder.0.id());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !std::fs::read(&cmdline).is_ok_and(|argv| argv.starts_with(b"git\0")) {
            assert!(Instant::now() < deadline, "git never showed in {cmdline}");
            std::thread::sleep(Duration::from_millis(5));
        }
        holder
    }
}

impl Drop for LockHolder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// A start that meets a live maintenance run leaves the ref locks to it; once the run ends, the
// first push fails on the stale lock, sweeps it on its way out, and the retry lands.
#[tokio::test(flavor = "multi_thread")]
async fn a_start_deferred_by_live_maintenance_is_swept_by_the_first_push() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(dir.path());
    let first = start(&config).await.unwrap();
    let root = api_admin(first.db()).await;
    let api = format!("http://{}/api/v1", first.http_addr());
    api_team(&api, &root, "core").await;
    api_workspace(&api, &root, "core", "api", false).await;
    first.shutdown().await;

    let repo = repo_path(dir.path(), "core", "api");
    let stale = repo.join("refs/heads/main.lock");
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::write(&stale, "x").unwrap();
    // Backdated past git's 12-hour horizon, so only the holder's open fd keeps it live.
    let lock = repo.join("objects/maintenance.lock");
    std::fs::File::create(&lock)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(13 * 3600))
        .unwrap();
    let holder = LockHolder::open(&lock);

    let again = start(&config).await.unwrap();
    write_hook(&dir.path().join("hooks"), "exit 0");
    assert!(stale.exists(), "the startup sweep defers to the live run");
    drop(holder);

    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = work_tree(&home, "work");
    let url = format!("http://{}/git/core/api.git", again.http_addr());
    let push = || {
        git(
            &home,
            Some(&work),
            Some(&root),
            &["push", "-q", &url, "main"],
        )
    };
    assert!(
        !push().status.success(),
        "the first push meets the stale lock"
    );
    // The push is answered before receive-pack is reaped and its writer leaves, so the sweep
    // lands a moment after the client returns.
    let deadline = Instant::now() + Duration::from_secs(10);
    while stale.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !stale.exists(),
        "the first push out sweeps the deferred lock"
    );
    ok(push());
    again.shutdown().await;
}

// A pre-receive that leaves a 1 s sleeper in git's process group keeps the reaper, and so the
// push record, waiting after git has answered; shutdown must not close the database under it.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_waits_for_the_record_of_a_push_git_already_answered() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(dir.path());
    let running = start(&config).await.unwrap();
    let root = api_admin(running.db()).await;
    let api = format!("http://{}/api/v1", running.http_addr());
    api_team(&api, &root, "core").await;
    api_workspace(&api, &root, "core", "api", false).await;
    write_hook(
        &dir.path().join("hooks"),
        "(sleep 1 </dev/null >/dev/null 2>&1 &)\nexit 0",
    );
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = work_tree(&home, "work");
    let url = format!("http://{}/git/core/api.git", running.http_addr());
    ok(git(
        &home,
        Some(&work),
        Some(&root),
        &["push", "-q", &url, "main"],
    ));
    running.shutdown().await;
    let db = potluck::db::Db::open(&config.db_path()).unwrap();
    let ws = db.workspace_by_slugs("core", "api").await.unwrap().unwrap();
    db.close();
    assert!(
        !ws.import_pending(),
        "the push record landed before shutdown returned"
    );
}

// A git that stalls 1 s on `commit-tree` holds a workspace create mid-seed; the client gives up
// and disconnects, so the listeners drain at once and only the task tracker can make shutdown
// wait. Shutdown must leave the workspace whole: its row and its repository.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_waits_for_a_workspace_create_whose_client_disconnected() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let marker = dir.path().join("seeding");
    let slow_git = dir.path().join("slow-git");
    write_script(
        &slow_git,
        &format!(
            "#!/bin/sh\ncase \" $* \" in *\" commit-tree \"*) : > '{}'; sleep 1;; esac\nexec git \"$@\"\n",
            marker.display()
        ),
    );
    let config = parse_config(&[
        "--listen",
        "127.0.0.1:0",
        "--data",
        data.to_str().unwrap(),
        "--public-url",
        "http://127.0.0.1:8080",
        "--git",
        slow_git.to_str().unwrap(),
    ])
    .unwrap();
    let running = start(&config).await.unwrap();
    let root = api_admin(running.db()).await;
    let api = format!("http://{}/api/v1", running.http_addr());
    api_team(&api, &root, "core").await;
    let request = tokio::spawn(async move {
        let body = serde_json::json!({"slug": "api", "name": "API", "kind": "apinata", "default_branch": "main"});
        reqwest::Client::new()
            .post(format!("{api}/teams/core/workspaces"))
            .bearer_auth(&root)
            .json(&body)
            .send()
            .await
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "the create never reached the seed commit"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Dropping the in-flight request closes its connection, as a client that gives up would.
    request.abort();
    let _ = request.await;
    running.shutdown().await;
    let db = potluck::db::Db::open(&config.db_path()).unwrap();
    let row = db.workspace_by_slugs("core", "api").await.unwrap();
    db.close();
    let repo = repo_path(&data, "core", "api");
    assert!(
        row.is_some(),
        "the create finished its row before shutdown returned"
    );
    assert!(repo.is_dir(), "the repository is kept with its row");
}

#[tokio::test]
async fn dropping_a_running_server_frees_the_data_dir_lock() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(dir.path());
    let running = start(&config).await.unwrap();
    let socket = running.admin_socket().to_path_buf();
    drop(running);
    // The detached listener tasks wind down on their own; once the admin socket stops
    // answering, a refused start can only be the lock.
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        assert!(
            Instant::now() < deadline,
            "the dropped server's admin socket kept accepting"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A sibling test's git child can hold a duplicate of a lock fd between fork and exec, so
    // `DataDirInUse` is retried briefly; a lock the dropped server kept never clears.
    let deadline = Instant::now() + Duration::from_secs(5);
    let again = loop {
        match start(&config).await {
            Err(ServerError::DataDirInUse { .. }) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(ServerError::DataDirInUse { .. }) => {
                panic!("the dropped server still holds the data directory lock")
            }
            started => {
                break started.expect("a new server starts on the dropped server's data directory");
            }
        }
    };
    again.shutdown().await;
}

#[tokio::test]
async fn a_start_empties_hook_reports_and_leaves_the_rest_of_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(dir.path());
    std::fs::create_dir_all(config.hook_reports_dir()).unwrap();
    let stale = config.hook_reports_dir().join("report-stale");
    std::fs::write(&stale, b"secret_env_file env_file\n").unwrap();
    let other = config.tmp_dir().join("keep-me");
    std::fs::write(&other, b"x").unwrap();
    let running = start(&config).await.unwrap();
    assert!(!stale.exists(), "a killed push's report is gone");
    assert!(other.exists(), "only hook-reports is emptied");
    assert!(config.hook_reports_dir().is_dir());
    running.shutdown().await;
}

// A row whose repository went missing and a repository with no row are both named at startup,
// by `team/ws` only, so the log never carries the data dir's absolute path.
#[test]
fn a_start_warns_about_rows_and_repositories_that_disagree() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(dir.path());
    // Current-thread, so the server's own events land in this thread's capture.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let first = start(&config).await.unwrap();
        let root = api_admin(first.db()).await;
        let api = format!("http://{}/api/v1", first.http_addr());
        api_team(&api, &root, "core").await;
        api_workspace(&api, &root, "core", "gone", false).await;
        first.shutdown().await;
    });
    std::fs::remove_dir_all(repo_path(dir.path(), "core", "gone")).unwrap();
    std::fs::create_dir_all(repo_path(dir.path(), "core", "stray")).unwrap();

    let text = crate::logs::captured_logs(|| {
        rt.block_on(async {
            let again = start(&config).await.unwrap();
            again.shutdown().await;
        });
    });
    let warns: Vec<&str> = text
        .lines()
        .filter(|line| line.contains(" WARN "))
        .collect();
    assert!(
        warns.iter().any(|line| line.contains("core/gone")),
        "the row without a repository is named: {text}"
    );
    assert!(
        warns.iter().any(|line| line.contains("core/stray")),
        "the repository without a row is named: {text}"
    );
    let data = dir.path().to_str().unwrap();
    assert!(!text.contains(data), "no absolute path is logged: {text}");
}
