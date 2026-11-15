//! Protocol selection, malformed requests and oversized bodies on the git routes, plus the
//! missing-directory 500.

use potluck::role::Role;
use potluck::slug::Slug;
use serde_json::Value;

use crate::common::{GitServer, TestAccess, failed, git, git_server, ok, work_tree};

const TOKEN: &str = "plk_writer_token_000000000000000000000000000";

async fn server(extra: &[&str]) -> GitServer {
    let access =
        TestAccess::new()
            .workspace("core", "api")
            .grant(TOKEN, "core", "api", Role::Write);
    let server = git_server(access, None, extra).await;
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

async fn json(res: reqwest::Response) -> (u16, Value) {
    (res.status().as_u16(), res.json().await.unwrap())
}

#[tokio::test]
async fn info_refs_advertises_v2_only_when_asked() {
    let server = server(&[]).await;
    let client = reqwest::Client::new();
    let url = format!(
        "{}/info/refs?service=git-upload-pack",
        server.url("core", "api")
    );
    let v2 = client
        .get(&url)
        .bearer_auth(TOKEN)
        .header("Git-Protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(v2.status().as_u16(), 200);
    assert_eq!(
        v2.headers()["content-type"],
        "application/x-git-upload-pack-advertisement"
    );
    assert_eq!(
        v2.headers()["cache-control"],
        "no-cache, max-age=0, must-revalidate"
    );
    let body = v2.bytes().await.unwrap();
    assert!(
        body.starts_with(b"000eversion 2\n"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    let v0 = client.get(&url).bearer_auth(TOKEN).send().await.unwrap();
    let body = v0.bytes().await.unwrap();
    assert!(
        body.starts_with(b"001e# service=git-upload-pack\n0000"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    let other = client
        .get(&url)
        .bearer_auth(TOKEN)
        .header("Git-Protocol", "version=2:foo=bar")
        .send()
        .await
        .unwrap();
    let body = other.bytes().await.unwrap();
    assert!(
        body.starts_with(b"001e# service"),
        "anything but exactly version=2 is v0: {}",
        String::from_utf8_lossy(&body)
    );
    let push = client
        .get(format!(
            "{}/info/refs?service=git-receive-pack",
            server.url("core", "api")
        ))
        .bearer_auth(TOKEN)
        .header("Git-Protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(
        push.headers()["content-type"],
        "application/x-git-receive-pack-advertisement"
    );
    assert!(
        push.bytes()
            .await
            .unwrap()
            .starts_with(b"001f# service=git-receive-pack\n0000"),
        "receive-pack keeps the service line under v2"
    );
}

// The blocking git binary talks to the in-process server, which needs a second worker thread.
#[tokio::test(flavor = "multi_thread")]
async fn a_v0_client_can_clone() {
    let server = server(&[]).await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    ok(git(
        &home,
        Some(&home),
        Some(TOKEN),
        &["-c", "protocol.version=0", "clone", "-q", &url, "v0"],
    ));
    assert!(home.join("v0").join("README").is_file());
}

#[tokio::test]
async fn info_refs_without_a_service_is_400_and_an_unknown_one_422() {
    let server = server(&[]).await;
    let client = reqwest::Client::new();
    let (status, body) = json(
        client
            .get(format!("{}/info/refs", server.url("core", "api")))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    let (status, body) = json(
        client
            .get(format!(
                "{}/info/refs?service=git-archive",
                server.url("core", "api")
            ))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, 422);
    assert_eq!(body["error"]["code"], "invalid_request");
    assert_eq!(body["error"]["field"], "service");
}

#[tokio::test]
async fn an_unsupported_content_encoding_is_400() {
    let server = server(&[]).await;
    let res = reqwest::Client::new()
        .post(format!("{}/git-upload-pack", server.url("core", "api")))
        .bearer_auth(TOKEN)
        .header("Content-Encoding", "br")
        .body("0014command=ls-refs\n0000")
        .send()
        .await
        .unwrap();
    let (status, body) = json(res).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("gzip"),
        "{body}"
    );
}

#[tokio::test]
async fn a_declared_length_over_the_cap_is_413_without_reading_the_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = server(&["--max-push-bytes", "1024"]).await;
    // A raw request that declares 64 MiB and sends none of it: the 413 must arrive anyway,
    // which proves the cap is checked on the header before the body is touched.
    let addr = server.base.trim_start_matches("http://").to_owned();
    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let request = format!(
        "POST /git/core/api.git/git-receive-pack HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/x-git-receive-pack-request\r\nContent-Length: 67108864\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&raw);
            if let Some((head, body)) = text.split_once("\r\n\r\n")
                && let Some(len) = head.lines().find_map(|l| {
                    l.strip_prefix("content-length: ")
                        .or_else(|| l.strip_prefix("Content-Length: "))
                })
                && body.len() >= len.trim().parse::<usize>().unwrap()
            {
                break;
            }
        }
    })
    .await;
    assert!(
        read.is_ok(),
        "the 413 arrived within 5 s although no body was sent"
    );
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 413 "), "{head}");
    let body: Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["error"]["code"], "payload_too_large");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("1024"),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chunked_push_over_the_cap_is_413() {
    let server = server(&["--max-push-bytes", "4096"]).await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    // 2 MiB of random bytes do not compress, so the pack is well over the cap and over
    // http.postBuffer, which makes git send it chunked (no Content-Length to refuse early).
    let mut big = vec![0u8; 2 * 1024 * 1024];
    for (i, b) in big.iter_mut().enumerate() {
        *b = (i as u32).wrapping_mul(2654435761).to_le_bytes()[3];
    }
    std::fs::write(work.join("big.bin"), big).unwrap();
    ok(git(&home, Some(&work), None, &["add", "big.bin"]));
    ok(git(
        &home,
        Some(&work),
        None,
        &["commit", "-q", "-m", "big"],
    ));
    let stderr = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    assert!(stderr.contains("413"), "{stderr}");
    let refs = ok(git(&home, Some(&home), Some(TOKEN), &["ls-remote", &url]));
    assert!(!refs.contains("refs/heads/main"), "nothing landed: {refs}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_small_push_passes_a_cap_it_fits_under() {
    let server = server(&["--max-push-bytes", "65536"]).await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
}

#[tokio::test]
async fn a_granted_repository_missing_on_disk_is_500() {
    let server = server(&[]).await;
    std::fs::remove_dir_all(
        server
            .repos
            .path(&Slug::parse("core").unwrap(), &Slug::parse("api").unwrap()),
    )
    .unwrap();
    let res = reqwest::Client::new()
        .get(format!(
            "{}/info/refs?service=git-upload-pack",
            server.url("core", "api")
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    let (status, body) = json(res).await;
    assert_eq!(status, 500);
    assert_eq!(body["error"]["code"], "internal");
    assert!(
        !body["error"]["message"]
            .as_str()
            .unwrap()
            .contains(server.dir.path().to_str().unwrap()),
        "no path in the body: {body}"
    );
}
