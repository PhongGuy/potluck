//! `AdminClient`: the socket probe, JSON bodies both ways, and empty answers.

use std::io::ErrorKind;
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};

use potluck::admin::client::{AdminClient, Verb};
use potluck::server::start;
use serde_json::{Value, json};

use crate::common::{client, config_for};

#[tokio::test]
async fn connect_reports_a_missing_socket_as_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let err = AdminClient::connect(&dir.path().join("admin.sock"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound, "got {err:?}");
}

#[tokio::test]
async fn connect_reports_a_dead_socket_file_as_connection_refused() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("admin.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // The duplicate stands in for a child forked by another test that holds the fd until exec.
    let forked_child_fd = listener.try_clone().unwrap();
    // Dropping our fd alone leaves the socket listening through that duplicate; shutdown(2) acts on
    // the shared socket, so it is dead whoever still holds an fd to it.
    UnixStream::from(OwnedFd::from(listener))
        .shutdown(Shutdown::Both)
        .unwrap();
    assert!(socket.exists(), "the file outlives the listener");
    let err = AdminClient::connect(&socket).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ConnectionRefused, "got {err:?}");
    drop(forked_child_fd);
}

#[tokio::test]
async fn get_health_answers_200_with_json() {
    let dir = tempfile::tempdir().unwrap();
    let running = start(&config_for(dir.path())).await.unwrap();
    let client = AdminClient::connect(running.admin_socket()).await.unwrap();
    let response = client
        .send(Verb::Get, "/api/v1/health", None)
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = serde_json::from_str(response.text()).unwrap();
    assert_eq!(body["status"], "ok");
    running.shutdown().await;
}

#[tokio::test]
async fn an_unknown_route_carries_the_error_body() {
    let dir = tempfile::tempdir().unwrap();
    let running = start(&config_for(dir.path())).await.unwrap();
    let client = AdminClient::connect(running.admin_socket()).await.unwrap();
    let response = client.send(Verb::Get, "/api/v1/nope", None).await.unwrap();
    assert_eq!(response.status(), 404);
    let body: Value = serde_json::from_str(response.text()).unwrap();
    assert_eq!(body["error"]["code"], "not_found");
    running.shutdown().await;
}

#[tokio::test]
async fn post_json_acts_as_system_and_a_delete_answers_an_empty_204() {
    let dir = tempfile::tempdir().unwrap();
    let running = start(&config_for(dir.path())).await.unwrap();
    let socket = AdminClient::connect(running.admin_socket()).await.unwrap();
    let body = json!({"username": "bob", "display_name": "Bob", "password": "pw-123456"});
    let created = socket
        .send(Verb::Post, "/api/v1/users", Some(&body.to_string()))
        .await
        .unwrap();
    assert_eq!(created.status(), 201, "{}", created.text());
    let user: Value = serde_json::from_str(created.text()).unwrap();
    assert_eq!(user["username"], "bob");
    // A token minted over TCP, revoked over the socket: a 204 has no body at all.
    let session: Value = client()
        .post(format!("http://{}/api/v1/session", running.http_addr()))
        .json(&json!({"username": "bob", "password": "pw-123456", "token_name": "t"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = session["token_meta"]["id"].as_i64().unwrap();
    let revoked = socket
        .send(Verb::Delete, &format!("/api/v1/tokens/{id}"), None)
        .await
        .unwrap();
    assert_eq!(revoked.status(), 204);
    assert_eq!(revoked.text(), "", "no body at all");
    running.shutdown().await;
}
