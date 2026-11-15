//! Both auth schemes, then 401, 403 and 404 on the git routes, through reqwest for the bodies
//! and through git for the client's behaviour. A denial never spawns git on the server.

use potluck::role::Role;
use potluck::slug::Slug;
use serde_json::Value;

use crate::common::{GitServer, TestAccess, failed, git, git_server, ok, work_tree};

const WRITER: &str = "plk_writer_token_000000000000000000000000000";
const READER: &str = "plk_reader_token_000000000000000000000000000";
const STRANGER: &str = "plk_stranger_token_0000000000000000000000000";

async fn server() -> GitServer {
    let access = TestAccess::new()
        .workspace("core", "api")
        .workspace("core", "private")
        .grant(WRITER, "core", "api", Role::Write)
        .grant(READER, "core", "api", Role::Read)
        .grant(STRANGER, "core", "private", Role::Read);
    let server = git_server(access, None, &[]).await;
    let s = |x| Slug::parse(x).unwrap();
    server
        .repos
        .init(&s("core"), &s("api"), "main")
        .await
        .unwrap();
    server
        .repos
        .init(&s("core"), &s("private"), "main")
        .await
        .unwrap();
    server
}

async fn get(url: &str, token: Option<&str>) -> (u16, reqwest::header::HeaderMap, Value) {
    let mut req = reqwest::Client::new().get(url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let res = req.send().await.unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    (status, headers, res.json().await.unwrap())
}

// Tests that run the blocking `git` binary need a second worker, or the in-process server
// never gets to answer it.
#[tokio::test(flavor = "multi_thread")]
async fn basic_auth_with_any_username_carries_the_token() {
    use base64::Engine;
    let server = server().await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("whoever:{WRITER}"));
    let out = std::process::Command::new("git")
        .args(["ls-remote", &url])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: Basic {basic}"),
        )
        .output()
        .unwrap();
    ok(out);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_askpass_client_is_challenged_then_admitted() {
    use std::os::unix::fs::PermissionsExt;
    let server = server().await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    ok(git(
        &home,
        Some(&work),
        Some(WRITER),
        &["push", "-q", &url, "main"],
    ));
    // git asks for a username, then a password; the password is the token.
    let askpass = home.join("askpass.sh");
    std::fs::write(
        &askpass,
        format!("#!/bin/sh\ncase \"$1\" in Username*) echo someone;; *) echo {WRITER};; esac\n"),
    )
    .unwrap();
    std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = std::process::Command::new("git")
        .args(["clone", "-q", &url, "asked"])
        .current_dir(&home)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", &askpass)
        .output()
        .unwrap();
    ok(out);
    assert!(
        home.join("asked").join("README").is_file(),
        "the 401 challenge led to Basic auth and a clone"
    );
}

#[tokio::test]
async fn no_credential_is_401_with_a_basic_challenge() {
    let server = server().await;
    let url = format!(
        "{}/info/refs?service=git-upload-pack",
        server.url("core", "api")
    );
    let (status, headers, body) = get(&url, None).await;
    assert_eq!(status, 401);
    assert_eq!(headers["www-authenticate"], "Basic realm=\"potluck\"");
    assert_eq!(body["error"]["code"], "unauthenticated");
    assert!(body["error"]["field"].is_null());
    let post = reqwest::Client::new()
        .post(format!("{}/git-upload-pack", server.url("core", "api")))
        .body("0000")
        .send()
        .await
        .unwrap();
    assert_eq!(
        post.status().as_u16(),
        401,
        "POST routes deny before reading the body"
    );
}

#[tokio::test]
async fn an_unknown_token_is_401_and_never_echoed() {
    let server = server().await;
    let url = format!(
        "{}/info/refs?service=git-upload-pack",
        server.url("core", "api")
    );
    let bogus = "plk_bogus_token_000000000000000000000000000";
    let (status, _, body) = get(&url, Some(bogus)).await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], "unauthenticated");
    assert!(!body.to_string().contains("bogus"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_role_can_fetch_but_not_push() {
    let server = server().await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    ok(git(
        &home,
        Some(&work),
        Some(WRITER),
        &["push", "-q", &url, "main"],
    ));
    ok(git(
        &home,
        Some(&home),
        Some(READER),
        &["clone", "-q", &url, "ro"],
    ));
    std::fs::write(home.join("ro").join("README"), "mine\n").unwrap();
    ok(git(
        &home,
        Some(&home.join("ro")),
        None,
        &["commit", "-q", "-am", "mine"],
    ));
    let stderr = failed(git(
        &home,
        Some(&home.join("ro")),
        Some(READER),
        &["push", "-q", "origin", "main"],
    ));
    assert!(stderr.contains("403"), "{stderr}");
    let (status, _, body) = get(
        &format!("{url}/info/refs?service=git-receive-pack"),
        Some(READER),
    )
    .await;
    assert_eq!(status, 403);
    assert_eq!(body["error"]["code"], "forbidden");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("read"),
        "{body}"
    );
    // The advertisement is pkt-lines, not JSON, so only the status is read.
    let res = reqwest::Client::new()
        .get(format!("{url}/info/refs?service=git-upload-pack"))
        .bearer_auth(READER)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200, "read may fetch");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_and_unreadable_workspaces_are_404() {
    let server = server().await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    for (team, ws) in [("core", "nope"), ("other", "api")] {
        let (status, _, body) = get(
            &format!("{}/info/refs?service=git-upload-pack", server.url(team, ws)),
            Some(WRITER),
        )
        .await;
        assert_eq!(status, 404, "{team}/{ws}");
        assert_eq!(body["error"]["code"], "not_found");
    }
    let (status, _, body) = get(
        &format!(
            "{}/info/refs?service=git-upload-pack",
            server.url("core", "private")
        ),
        Some(WRITER),
    )
    .await;
    assert_eq!(status, 404, "a workspace without a grant looks unknown");
    assert_eq!(body["error"]["code"], "not_found");
    let stderr = failed(git(
        &home,
        Some(&home),
        Some(WRITER),
        &["clone", "-q", &server.url("core", "private"), "x"],
    ));
    assert!(stderr.contains("not found"), "{stderr}");
}

#[tokio::test]
async fn bad_slugs_and_a_missing_git_suffix_are_404() {
    let server = server().await;
    for path in [
        "/git/core/api/info/refs?service=git-upload-pack",
        "/git/core/api.git.git/info/refs?service=git-upload-pack",
        "/git/Core/api.git/info/refs?service=git-upload-pack",
        "/git/core/..%2Fapi.git/info/refs?service=git-upload-pack",
        "/git/-core/api.git/info/refs?service=git-upload-pack",
        "/git/core/%FF.git/info/refs?service=git-upload-pack",
    ] {
        let (status, _, body) = get(&format!("{}{path}", server.base), Some(WRITER)).await;
        assert_eq!(status, 404, "{path}");
        assert_eq!(body["error"]["code"], "not_found", "{path}");
    }
}
