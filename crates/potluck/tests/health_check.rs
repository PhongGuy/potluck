//! `potluck health-check` through the binary against a real `potluck serve` child.

use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use crate::common::potluck;

/// Kills the server child when a test ends, pass or fail.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_server(data: &Path) -> Server {
    let child = potluck()
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--public-url",
            "http://127.0.0.1:8080",
            "--data",
        ])
        .arg(data)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn potluck serve");
    let socket = data.join("admin.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "admin.sock did not appear within 10 s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Server(child)
}

#[test]
fn health_check_passes_against_a_running_server() {
    let dir = tempfile::tempdir().unwrap();
    let _server = spawn_server(dir.path());
    let out = potluck()
        .args(["health-check", "--data"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
}

#[test]
fn health_check_reads_the_data_dir_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let _server = spawn_server(dir.path());
    let out = potluck()
        .env("POTLUCK_DATA", dir.path())
        .arg("health-check")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn health_check_fails_when_no_server_runs() {
    let dir = tempfile::tempdir().unwrap();
    let out = potluck()
        .args(["health-check", "--data"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("admin.sock"), "{stderr}");
    assert_eq!(
        stderr.matches("No such file").count(),
        1,
        "the cause is printed once: {stderr}"
    );
}

#[test]
fn health_check_fails_after_the_server_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let server = spawn_server(dir.path());
    drop(server);
    assert!(
        dir.path().join("admin.sock").exists(),
        "SIGKILL leaves the socket file behind"
    );
    let out = potluck()
        .args(["health-check", "--data"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "a dead socket file is not healthy"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("admin.sock"));
}
