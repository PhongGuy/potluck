//! Real `git push` through the server against the installed hook.

use std::path::Path;
use std::time::Duration;

use potluck::git::GitLimits;
use potluck::role::Role;
use potluck::server::start;
use potluck::slug::Slug;

use crate::common::{
    APINATA_FORMAT, GitServer, TestAccess, api_admin, api_team, api_workspace, client, failed, git,
    git_server, ok, parse_config, work_tree, write_script,
};

const TOKEN: &str = "plk_writer_token_000000000000000000000000000";
const ENV_YML: &[u8] = include_bytes!("fixtures/secret-check/bad/env-secret-valued.yml");
const AWS: &[u8] = include_bytes!("fixtures/secret-check/bad/aws-access-key.txt");

/// A server whose only workspace is `core/api` of kind apinata, with `extra_env` for the hook.
async fn apinata_server(extra_env: &[(&str, &str)]) -> (GitServer, std::path::PathBuf, String) {
    let mut access = TestAccess::new()
        .workspace("core", "api")
        .grant(TOKEN, "core", "api", Role::Write)
        .hook_env("POTLUCK_KIND", "apinata");
    for (k, v) in extra_env {
        access = access.hook_env(k, v);
    }
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
    (server, home, url)
}

fn repo(server: &GitServer) -> std::path::PathBuf {
    server
        .repos
        .path(&Slug::parse("core").unwrap(), &Slug::parse("api").unwrap())
}

fn server_main(server: &GitServer, home: &Path) -> Option<String> {
    let out = git(
        home,
        Some(home),
        None,
        &[
            "--git-dir",
            repo(server).to_str().unwrap(),
            "rev-parse",
            "-q",
            "--verify",
            "refs/heads/main",
        ],
    );
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The work tree's `HEAD`, the commit an accepted push of `main` leaves on the server.
fn work_head(home: &Path, work: &Path) -> String {
    ok(git(home, Some(work), None, &["rev-parse", "HEAD"]))
        .trim()
        .to_owned()
}

fn commit_all(home: &Path, work: &Path, message: &str) {
    ok(git(home, Some(work), None, &["add", "-A"]));
    ok(git(
        home,
        Some(work),
        None,
        &["commit", "-q", "-m", message],
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_with_secrets_is_declined_with_the_contract_lines_and_a_clean_one_lands() {
    let (server, home, url) = apinata_server(&[]).await;
    let work = work_tree(&home, "work");
    std::fs::write(work.join("opencollection.yml"), APINATA_FORMAT).unwrap();
    commit_all(&home, &work, "format");
    std::fs::write(work.join(".env"), "KEY=fixture\n").unwrap();
    std::fs::create_dir_all(work.join("environments")).unwrap();
    std::fs::write(work.join("environments").join("dev.yml"), ENV_YML).unwrap();
    commit_all(&home, &work, "secrets");
    let output = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "--porcelain", &url, "main"],
    ));
    assert!(
        output.contains("remote: potluck-reject secret_env_file refs/heads/main .env env_file"),
        "{output}"
    );
    assert!(
        output.contains(
            "remote: potluck-reject secret_value refs/heads/main environments/dev.yml secret_value"
        ),
        "{output}"
    );
    assert!(output.contains("pre-receive hook declined"), "{output}");
    assert!(
        !output.contains("fixture-not-a-real-secret"),
        "the matched text never comes back: {output}"
    );
    assert_eq!(server_main(&server, &home), None, "nothing landed");
    // Removing the files in a later commit leaves them in the pushed history: still refused.
    std::fs::remove_file(work.join(".env")).unwrap();
    std::fs::remove_file(work.join("environments").join("dev.yml")).unwrap();
    commit_all(&home, &work, "remove");
    let output = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "--porcelain", &url, "main"],
    ));
    assert_eq!(server_main(&server, &home), None, "still nothing landed");
    assert!(
        output.contains("potluck-reject secret_env_file"),
        "{output}"
    );
    // Back to the clean README commit: lands.
    ok(git(
        &home,
        Some(&work),
        None,
        &["reset", "-q", "--hard", "HEAD~2"],
    ));
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    assert_eq!(server_main(&server, &home), Some(work_head(&home, &work)));
}

#[tokio::test(flavor = "multi_thread")]
async fn history_already_on_the_server_does_not_block_later_pushes() {
    let (server, home, url) = apinata_server(&[]).await;
    let work = work_tree(&home, "work");
    std::fs::write(work.join("creds.txt"), AWS).unwrap();
    commit_all(&home, &work, "legacy secret");
    // Placed on the server directly, as an import or an older release would have left it.
    ok(git(
        &home,
        Some(&home),
        None,
        &[
            "--git-dir",
            repo(&server).to_str().unwrap(),
            "fetch",
            "-q",
            work.to_str().unwrap(),
            "+main:refs/heads/main",
        ],
    ));
    std::fs::write(work.join("README"), "later\n").unwrap();
    commit_all(&home, &work, "later");
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    assert_eq!(server_main(&server, &home), Some(work_head(&home, &work)));
}

#[tokio::test(flavor = "multi_thread")]
async fn import_mode_scans_the_tip_tree_only() {
    let (server, home, url) = apinata_server(&[("POTLUCK_IMPORT", "1")]).await;
    let work = work_tree(&home, "work");
    std::fs::write(work.join("opencollection.yml"), APINATA_FORMAT).unwrap();
    std::fs::write(work.join("creds.txt"), AWS).unwrap();
    commit_all(&home, &work, "add");
    std::fs::remove_file(work.join("creds.txt")).unwrap();
    commit_all(&home, &work, "remove");
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    let imported = work_head(&home, &work);
    assert_eq!(server_main(&server, &home), Some(imported.clone()));
    std::fs::write(work.join(".env"), "x\n").unwrap();
    commit_all(&home, &work, "env in tip");
    let output = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "--porcelain", &url, "main"],
    ));
    assert_eq!(
        server_main(&server, &home),
        Some(imported),
        "the ref did not move"
    );
    assert!(
        output.contains("remote: potluck-reject secret_env_file refs/heads/main .env env_file"),
        "{output}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_armed_override_passes_the_push_unscanned() {
    let (server, home, url) = apinata_server(&[("POTLUCK_OVERRIDE", "7")]).await;
    let work = work_tree(&home, "work");
    std::fs::write(work.join(".env"), "x\n").unwrap();
    commit_all(&home, &work, "env");
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    assert_eq!(server_main(&server, &home), Some(work_head(&home, &work)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_hook_file_fails_the_push_with_500() {
    let (server, home, url) = apinata_server(&[]).await;
    let work = work_tree(&home, "work");
    std::fs::write(work.join("opencollection.yml"), APINATA_FORMAT).unwrap();
    commit_all(&home, &work, "format");
    let hook = server.dir.path().join("hooks").join("pre-receive");
    std::fs::remove_file(&hook).unwrap();
    let output = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    assert!(output.contains("500"), "{output}");
    assert_eq!(
        server_main(&server, &home),
        None,
        "nothing landed unchecked"
    );
    ok(git(&home, Some(&home), Some(TOKEN), &["ls-remote", &url]));
    crate::common::write_script(
        &hook,
        &potluck::hooks::script(Path::new(env!("CARGO_BIN_EXE_potluck")), Path::new("git"))
            .unwrap(),
    );
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blob_moved_into_environments_is_refused_under_its_new_path() {
    let (server, home, url) = apinata_server(&[]).await;
    let work = work_tree(&home, "work");
    std::fs::write(work.join("opencollection.yml"), APINATA_FORMAT).unwrap();
    std::fs::create_dir_all(work.join("notes")).unwrap();
    std::fs::write(work.join("notes").join("x.txt"), ENV_YML).unwrap();
    commit_all(&home, &work, "notes");
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    let landed = work_head(&home, &work);
    assert_eq!(server_main(&server, &home), Some(landed.clone()));
    std::fs::remove_file(work.join("notes").join("x.txt")).unwrap();
    std::fs::create_dir_all(work.join("environments")).unwrap();
    std::fs::write(work.join("environments").join("dev.yml"), ENV_YML).unwrap();
    commit_all(&home, &work, "move");
    let output = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "--porcelain", &url, "main"],
    ));
    assert!(
        output.contains(
            "remote: potluck-reject secret_value refs/heads/main environments/dev.yml secret_value"
        ),
        "{output}"
    );
    assert_eq!(
        server_main(&server, &home),
        Some(landed),
        "the ref did not move"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_environment_file_over_the_cap_is_refused_but_a_big_note_lands() {
    let (server, home, url) = apinata_server(&[]).await;
    let work = work_tree(&home, "work");
    std::fs::write(work.join("opencollection.yml"), APINATA_FORMAT).unwrap();
    commit_all(&home, &work, "format");
    let big = vec![b'a'; 1024 * 1024 + 1];
    std::fs::create_dir_all(work.join("environments")).unwrap();
    std::fs::write(work.join("environments").join("big.yml"), &big).unwrap();
    commit_all(&home, &work, "big env");
    let output = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "--porcelain", &url, "main"],
    ));
    assert!(
        output.contains(
            "remote: potluck-reject secret_value refs/heads/main environments/big.yml env_unparsable"
        ),
        "{output}"
    );
    assert_eq!(server_main(&server, &home), None, "nothing landed");
    ok(git(
        &home,
        Some(&work),
        None,
        &["reset", "-q", "--hard", "HEAD~1"],
    ));
    std::fs::create_dir_all(work.join("notes")).unwrap();
    std::fs::write(work.join("notes").join("big.bin"), &big).unwrap();
    commit_all(&home, &work, "big note");
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    assert_eq!(server_main(&server, &home), Some(work_head(&home, &work)));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_configured_git_reaches_the_installed_hook() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("git.log");
    let wrapper = dir.path().join("logging-git");
    write_script(
        &wrapper,
        &format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nexec git \"$@\"\n",
            log.display()
        ),
    );
    let data = dir.path().join("data");
    let config = parse_config(&[
        "--listen",
        "127.0.0.1:0",
        "--data",
        data.to_str().unwrap(),
        "--public-url",
        "http://127.0.0.1:8080",
        "--git",
        wrapper.to_str().unwrap(),
    ])
    .unwrap();
    let running = start(&config).await.unwrap();
    // The installed hook execs this test binary; the potluck binary takes its place and the
    // rest of the script, the configured git included, stays as `start` wrote it.
    let hook = data.join("hooks").join("pre-receive");
    let installed = std::fs::read_to_string(&hook).unwrap();
    assert!(
        installed.contains(&format!("POTLUCK_GIT='{}'", wrapper.display())),
        "{installed}"
    );
    let this = std::env::current_exe().unwrap();
    let swapped = installed.replace(
        &format!("'{}'", this.display()),
        &format!("'{}'", env!("CARGO_BIN_EXE_potluck")),
    );
    assert_ne!(
        swapped, installed,
        "the installed hook names this test binary"
    );
    write_script(&hook, &swapped);
    let api = format!("http://{}/api/v1", running.http_addr());
    let root = api_admin(running.db()).await;
    api_team(&api, &root, "core").await;
    api_workspace(&api, &root, "core", "api", false).await;
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = work_tree(&home, "work");
    std::fs::write(work.join("opencollection.yml"), APINATA_FORMAT).unwrap();
    commit_all(&home, &work, "format");
    let url = format!("http://{}/git/core/api.git", running.http_addr());
    let pushed = tokio::task::spawn_blocking(move || {
        git(
            &home,
            Some(&work),
            Some(&root),
            &["push", "-q", &url, "main"],
        )
    })
    .await
    .unwrap();
    ok(pushed);
    running.shutdown().await;
    // Only the hook's scan runs `cat-file`; receive-pack's own helpers bypass `--git`.
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.lines().any(|l| l.starts_with("cat-file ")), "{calls}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_hook_is_refused_before_waiting_for_a_git_slot() {
    let access =
        TestAccess::new()
            .workspace("core", "api")
            .grant(TOKEN, "core", "api", Role::Write);
    // One slot and a 300 ms queue wait: a request that waited for the held slot would get 503.
    let limits = GitLimits::new(1, Duration::from_millis(300), Duration::from_secs(60));
    let server = git_server(access, Some(limits), &[]).await;
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
    let hook = server.dir.path().join("hooks").join("pre-receive");
    let started = server.dir.path().join("hook-started");
    write_script(
        &hook,
        &format!(
            "#!/bin/sh\ntouch '{}'\nsleep 5\nexit 0\n",
            started.display()
        ),
    );
    let (holder_home, holder_work, holder_url) = (home.clone(), work.clone(), url.clone());
    let holder = std::thread::spawn(move || {
        git(
            &holder_home,
            Some(&holder_work),
            Some(TOKEN),
            &["push", "-q", &holder_url, "main"],
        )
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !started.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the holding hook never ran"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The running hook keeps its open script; the next receive-pack finds no hook.
    std::fs::remove_file(&hook).unwrap();
    let output = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main:refs/heads/other"],
    ));
    assert!(output.contains("500"), "the advertisement: {output}");
    let posted = client()
        .post(format!("{url}/git-receive-pack"))
        .bearer_auth(TOKEN)
        .body("0000")
        .send()
        .await
        .unwrap();
    assert_eq!(posted.status().as_u16(), 500, "the receive-pack POST");
    ok(holder.join().unwrap());
}
