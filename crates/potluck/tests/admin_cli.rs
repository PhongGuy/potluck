//! `potluck admin` through the binary: help, the stdin password, exit codes, and a clean
//! stderr and server log.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use crate::common::potluck;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `potluck serve --log trace`, stderr to `<data>/server.log`; returns once the socket exists.
fn spawn_server(data: &Path) -> Server {
    let log = File::create(data.join("server.log")).unwrap();
    let child = potluck()
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--public-url",
            "http://127.0.0.1:8080",
            "--log",
            "trace",
            "--data",
        ])
        .arg(data)
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
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
fn admin_help_lists_every_noun_and_the_shared_flags() {
    let out = potluck().args(["admin", "--help"]).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "user",
        "token",
        "team",
        "workspace",
        "grant",
        "override",
        "--data",
        "--json",
        "POTLUCK_DATA",
    ] {
        assert!(text.contains(needle), "help lacks {needle}: {text}");
    }
}

#[test]
fn user_create_reads_the_password_from_stdin_and_keeps_it_off_stderr_and_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let server = spawn_server(dir.path());
    let mut child = potluck()
        .args(["admin", "--data"])
        .arg(dir.path())
        .args([
            "user",
            "create",
            "alice",
            "--server-admin",
            "--password-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hunter2-hunter2\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("alice") && stdout.contains("yes"),
        "the admin row: {stdout}"
    );
    assert!(!stdout.contains("hunter2"), "{stdout}");
    assert_eq!(out.stderr, b"", "stderr is silent on success");
    drop(server);
    let log = std::fs::read_to_string(dir.path().join("server.log")).unwrap();
    assert!(!log.is_empty(), "trace logging wrote something");
    assert!(
        !log.contains("hunter2"),
        "the password never reaches the server log"
    );
}

#[test]
fn a_generated_password_is_printed_once_and_stays_off_stderr_and_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let server = spawn_server(dir.path());
    let verbs: [&[&str]; 2] = [
        &["user", "create", "alice", "--server-admin"],
        &["user", "reset-password", "alice"],
    ];
    let mut passwords = Vec::new();
    for verb in verbs {
        let out = potluck()
            .args(["admin", "--data"])
            .arg(dir.path())
            .args(verb)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stderr, b"", "stderr is silent on success");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let password = stdout
            .lines()
            .find_map(|l| l.strip_prefix("password: "))
            .expect("a password line")
            .to_owned();
        assert_eq!(password.len(), 32, "{stdout}");
        assert_eq!(
            stdout.matches(&password).count(),
            1,
            "printed once: {stdout}"
        );
        passwords.push(password);
    }
    assert_ne!(passwords[0], passwords[1], "a reset mints a new password");
    drop(server);
    let log = std::fs::read_to_string(dir.path().join("server.log")).unwrap();
    assert!(!log.is_empty(), "trace logging wrote something");
    for password in &passwords {
        assert!(
            !log.contains(password.as_str()),
            "a generated password never reaches the server log"
        );
    }
}

#[test]
fn admin_without_a_server_exits_1_and_names_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let out = potluck()
        .args(["admin", "--data"])
        .arg(dir.path())
        .args(["user", "list"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("error: ") && stderr.contains("admin.sock"),
        "{stderr}"
    );
    assert_eq!(out.stdout, b"");
}

#[test]
fn a_usage_error_exits_2() {
    let out = potluck()
        .args(["admin", "user", "create"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "a missing username is clap's error"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("USERNAME"));
}
