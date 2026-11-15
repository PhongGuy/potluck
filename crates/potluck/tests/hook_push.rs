//! Real `git push` through the server against the installed hook.

use std::path::Path;

use potluck::role::Role;
use potluck::slug::Slug;

use crate::common::{GitServer, TestAccess, failed, git, git_server, ok, work_tree};

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
    let head = ok(git(&home, Some(&work), None, &["rev-parse", "HEAD"]))
        .trim()
        .to_owned();
    assert_eq!(server_main(&server, &home), Some(head));
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
}

#[tokio::test(flavor = "multi_thread")]
async fn import_mode_scans_the_tip_tree_only() {
    let (_server, home, url) = apinata_server(&[("POTLUCK_IMPORT", "1")]).await;
    let work = work_tree(&home, "work");
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
    std::fs::write(work.join(".env"), "x\n").unwrap();
    commit_all(&home, &work, "env in tip");
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
}

#[tokio::test(flavor = "multi_thread")]
async fn an_armed_override_passes_the_push_unscanned() {
    let (_server, home, url) = apinata_server(&[("POTLUCK_OVERRIDE", "7")]).await;
    let work = work_tree(&home, "work");
    std::fs::write(work.join(".env"), "x\n").unwrap();
    commit_all(&home, &work, "env");
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_hook_file_fails_the_push_with_500() {
    let (server, home, url) = apinata_server(&[]).await;
    let work = work_tree(&home, "work");
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
    potluck::hooks::install(
        hook.parent().unwrap(),
        Path::new(env!("CARGO_BIN_EXE_potluck")),
        Path::new("git"),
    )
    .unwrap();
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
}
