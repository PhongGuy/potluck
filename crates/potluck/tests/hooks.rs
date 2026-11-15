//! The hook stub is installed at start, `core.hooksPath` is what receive-pack runs, fsck is on,
//! and a busy engine answers 503.

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use potluck::git::GitLimits;
use potluck::hooks::install;
use potluck::role::Role;
use potluck::server::start;
use potluck::slug::Slug;

use crate::common::{
    GitServer, TestAccess, config_for, failed, git, git_server, git_server_at, ok, unix_get,
    work_tree,
};

const TOKEN: &str = "plk_writer_token_000000000000000000000000000";

/// `<data>/hooks`, which is what `Config::hooks_dir()` yields for the server's data dir.
fn hooks_dir(server: &GitServer) -> std::path::PathBuf {
    server.dir.path().join("hooks")
}

/// `dir` as a path relative to the working directory: `../` per cwd component, then the
/// absolute path without its leading slash. Linux only, which is what the suite runs on.
fn relative_to_cwd(dir: &std::path::Path) -> String {
    let cwd = std::env::current_dir().unwrap();
    let ups = "../".repeat(cwd.components().count() - 1);
    format!("{ups}{}", dir.strip_prefix("/").unwrap().display())
}

#[test]
fn install_writes_an_executable_stub_and_replaces_an_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let hook = dir.path().join("pre-receive");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    let written = install(dir.path()).unwrap();
    assert_eq!(written, hook);
    let text = std::fs::read_to_string(&hook).unwrap();
    assert!(text.starts_with("#!/bin/sh\n"), "{text}");
    assert!(text.ends_with("exit 0\n"), "{text}");
    assert_eq!(
        std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(
        !hook.with_extension("tmp").exists(),
        "the temporary name is gone"
    );
}

#[tokio::test]
async fn start_installs_the_hook_creates_tmp_and_guards_git_on_the_admin_socket() {
    let dir = tempfile::tempdir().unwrap();
    let running = start(&config_for(dir.path())).await.unwrap();
    let hook = dir.path().join("hooks").join("pre-receive");
    assert!(hook.is_file(), "hooks/pre-receive exists after start");
    assert_eq!(
        std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(
        dir.path().join("tmp").is_dir(),
        "the spool directory exists after start"
    );
    // The admin socket serves the same router; without a credential the git routes are 401
    // there too, so the `system` actor never bypasses the resolver.
    let (status, body) = unix_get(
        running.admin_socket(),
        "/git/core/api.git/info/refs?service=git-upload-pack",
    )
    .await;
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("unauthenticated"), "{body}");
    running.shutdown().await;
}

async fn writer_server(limits: Option<GitLimits>) -> GitServer {
    let access =
        TestAccess::new()
            .workspace("core", "api")
            .grant(TOKEN, "core", "api", Role::Write);
    let server = git_server(access, limits, &[]).await;
    server
        .repos
        .init(
            &Slug::parse("core").unwrap(),
            &Slug::parse("api").unwrap(),
            "main",
        )
        .await
        .unwrap();
    server
}

fn write_hook(dir: &std::path::Path, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let hook = dir.join("pre-receive");
    std::fs::write(&hook, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn receive_pack_runs_the_hook_from_hooks_path_not_the_repository() {
    let server = writer_server(None).await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    // The repository's own hook would accept; the data-dir hook declines. The data-dir one wins.
    let repo = server
        .repos
        .path(&Slug::parse("core").unwrap(), &Slug::parse("api").unwrap());
    write_hook(&repo.join("hooks"), "exit 0");
    write_hook(&hooks_dir(&server), "echo 'potluck says no' >&2\nexit 1");
    let output = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "--porcelain", &url, "main"],
    ));
    assert!(
        output.contains("remote: potluck says no"),
        "the hook's stderr reaches the client: {output}"
    );
    assert!(
        output.contains("pre-receive hook declined"),
        "porcelain reports the rejection on stdout: {output}"
    );
    write_hook(&hooks_dir(&server), "exit 0");
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relative_data_dir_still_runs_the_hook() {
    // `--data` as seen from the test's cwd; `Config` makes it absolute, so `core.hooksPath` is
    // absolute too. Without that, git resolves a relative hooksPath against the repository and
    // the declining hook below is never run (measured).
    // The data dir sits deeper than the cwd, so the `../` prefix cannot overshoot `/` and land
    // on the right directory from inside the repository by accident.
    let dir = tempfile::tempdir().unwrap();
    let cwd_depth = std::env::current_dir().unwrap().components().count();
    let data = (0..cwd_depth).fold(dir.path().to_path_buf(), |d, i| d.join(format!("d{i}")));
    std::fs::create_dir_all(&data).unwrap();
    let relative = relative_to_cwd(&data);
    assert!(!relative.starts_with('/'), "{relative}");
    let access =
        TestAccess::new()
            .workspace("core", "api")
            .grant(TOKEN, "core", "api", Role::Write);
    let server = git_server_at(dir, &relative, access, None, &[]).await;
    server
        .repos
        .init(
            &Slug::parse("core").unwrap(),
            &Slug::parse("api").unwrap(),
            "main",
        )
        .await
        .unwrap();
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    write_hook(&data.join("hooks"), "exit 1");
    let output = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "--porcelain", &url, "main"],
    ));
    assert!(output.contains("pre-receive hook declined"), "{output}");
}

#[tokio::test(flavor = "multi_thread")]
async fn hook_variables_from_access_reach_the_hook() {
    let access = TestAccess::new()
        .workspace("core", "api")
        .grant(TOKEN, "core", "api", Role::Write)
        .hook_env("POTLUCK_TEST_MARK", "from-the-resolver");
    let server = git_server(access, None, &[]).await;
    server
        .repos
        .init(
            &Slug::parse("core").unwrap(),
            &Slug::parse("api").unwrap(),
            "main",
        )
        .await
        .unwrap();
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    write_hook(
        &hooks_dir(&server),
        "echo \"mark=$POTLUCK_TEST_MARK home=$HOME\" >&2\nexit 1",
    );
    let output = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "--porcelain", &url, "main"],
    ));
    assert!(output.contains("mark=from-the-resolver"), "{output}");
    assert!(
        !output.contains("home=/"),
        "env_clear left no HOME for the hook: {output}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn receive_fsck_objects_rejects_a_commit_without_an_email() {
    let server = writer_server(None).await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    write_hook(&hooks_dir(&server), "exit 0");
    let tree = ok(git(&home, Some(&work), None, &["write-tree"]))
        .trim()
        .to_owned();
    let commit_text =
        format!("tree {tree}\nauthor A 1700000000 +0000\ncommitter A 1700000000 +0000\n\nbad\n");
    std::fs::write(work.join("commit.txt"), commit_text).unwrap();
    let sha = {
        let out = std::process::Command::new("git")
            .args([
                "hash-object",
                "-t",
                "commit",
                "-w",
                "--literally",
                "commit.txt",
            ])
            .current_dir(&work)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        ok(out).trim().to_owned()
    };
    ok(git(
        &home,
        Some(&work),
        None,
        &["update-ref", "refs/heads/bad", &sha],
    ));
    let output = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "--porcelain", &url, "bad"],
    ));
    assert!(
        output.contains("missingEmail"),
        "fsck names the problem: {output}"
    );
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_engine_answers_503_after_the_queue_wait() {
    let server = writer_server(Some(GitLimits::new(
        1,
        Duration::from_millis(300),
        Duration::from_secs(60),
    )))
    .await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    // The hook holds the only slot for 4 s; a concurrent ls-remote waits 300 ms and gets 503.
    write_hook(&hooks_dir(&server), "sleep 4\nexit 0");
    let push_home = home.clone();
    let push_url = url.clone();
    let push = std::thread::spawn(move || {
        git(
            &push_home,
            Some(&work),
            Some(TOKEN),
            &["push", "-q", &push_url, "main"],
        )
    });
    std::thread::sleep(Duration::from_millis(1500));
    let stderr = failed(git(&home, Some(&home), Some(TOKEN), &["ls-remote", &url]));
    assert!(stderr.contains("503"), "{stderr}");
    ok(push.join().unwrap());
    ok(git(&home, Some(&home), Some(TOKEN), &["ls-remote", &url]));
}
