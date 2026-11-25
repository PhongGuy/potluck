//! Text from outside the server reaches log lines with its control and format characters inert.

use serde_json::json;

use crate::common::{client, parse_config};
use crate::logs::captured_logs;

// Signs in once as `username` against a fresh server on a current-thread runtime, so the
// handler's events land in this thread's capture, and returns everything logged.
fn logs_of_a_refused_sign_in(username: &str) -> String {
    captured_logs(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let config = parse_config(&[
                "--listen",
                "127.0.0.1:0",
                "--data",
                dir.path().to_str().unwrap(),
                "--public-url",
                "http://127.0.0.1:8080",
            ])
            .unwrap();
            let running = potluck::server::start(&config).await.unwrap();
            let url = format!("http://{}/api/v1/session", running.http_addr());
            let body = json!({"username": username, "password": "x", "token_name": "probe"});
            let status = client()
                .post(&url)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status();
            assert_eq!(status.as_u16(), 401);
        });
    })
}

#[test]
fn a_bidi_override_in_a_sign_in_username_is_logged_escaped() {
    let text = logs_of_a_refused_sign_in("eve\u{202E}gnp.exe");
    let line = text
        .lines()
        .find(|line| line.contains("sign-in refused"))
        .unwrap_or_else(|| panic!("the refusal is logged: {text}"));
    assert!(!line.contains('\u{202E}'), "raw override in: {line:?}");
    assert!(
        line.contains(r"eve\u{202e}gnp.exe"),
        "escaped form in: {line:?}"
    );
}

#[test]
fn a_newline_in_a_sign_in_username_cannot_forge_a_log_line() {
    let text = logs_of_a_refused_sign_in("eve\n2026-01-01T00:00:00Z  INFO forged");
    assert!(
        !text.lines().any(|line| line.starts_with("2026-01-01")),
        "a forged line: {text}"
    );
    assert!(text.contains(r"eve\n2026-01-01"), "escaped newline: {text}");
}
