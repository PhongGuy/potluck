//! Shared helpers. Every test reaches only 127.0.0.1 or a Unix socket under a temp dir.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The built `potluck` binary with an empty environment, so no `POTLUCK_*` variable or
/// real config leaks in. Tests add what they need with `.env(..)`.
pub fn potluck() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_potluck"));
    cmd.env_clear();
    cmd
}

use clap::Parser;
use potluck::config::{Config, ConfigError, ServeArgs};
use potluck::db::Db;

#[derive(Parser)]
struct ServeWrap {
    #[command(flatten)]
    serve: ServeArgs,
}

/// Builds a `Config` from `potluck serve` flags. Pass every flag explicitly: clap also reads
/// `POTLUCK_*` from the real environment, and these tests must not depend on it.
pub fn parse_config(args: &[&str]) -> Result<Config, ConfigError> {
    let argv = std::iter::once("potluck").chain(args.iter().copied());
    let wrap = ServeWrap::try_parse_from(argv).expect("flags parse");
    Config::try_from(wrap.serve)
}

/// A `Config` for a server on an ephemeral loopback port with `dir` as its data directory.
pub fn config_for(dir: &Path) -> Config {
    let data = dir.to_str().expect("utf-8 temp path");
    parse_config(&[
        "--listen",
        "127.0.0.1:0",
        "--data",
        data,
        "--public-url",
        "http://127.0.0.1:8080",
    ])
    .expect("test config")
}

/// Serves `router` on an ephemeral loopback port for the life of the returned task.
pub async fn serve(router: axum::Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    (addr, task)
}

/// A migrated database in its own temp dir.
pub async fn migrated_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Db::open(&dir.path().join("server.db")).expect("open");
    db.migrate().await.expect("migrate");
    (dir, db)
}

/// One HTTP/1.1 GET over a Unix socket; returns the status code and the body.
pub async fn unix_get(socket: &Path, target: &str) -> (u16, String) {
    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .expect("connect admin socket");
    let request = format!("GET {target} HTTP/1.1\r\nHost: potluck\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut raw = String::new();
    stream
        .read_to_string(&mut raw)
        .await
        .expect("read response");
    let (head, body) = raw.split_once("\r\n\r\n").expect("header terminator");
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code");
    (status, body.to_string())
}
