//! compose.yaml and deploy/potluck.container: the invariants both share, a compose
//! validation when a compose tool is installed, and a Quadlet dry run when podman's
//! generator is installed. Nothing here starts a container or touches user config.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::common::repo_root;

const IMAGE: &str = "ghcr.io/phongguy/potluck:latest";

fn compose() -> String {
    fs::read_to_string(repo_root().join("compose.yaml")).unwrap()
}

fn quadlet() -> String {
    fs::read_to_string(repo_root().join("deploy/potluck.container")).unwrap()
}

fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

#[test]
fn both_files_run_the_image_on_loopback_with_one_volume_and_no_secret() {
    for (name, text) in [
        ("compose.yaml", compose()),
        ("potluck.container", quadlet()),
    ] {
        assert!(text.contains(IMAGE), "{name}: image");
        assert!(
            text.contains("127.0.0.1:8080:8080"),
            "{name}: port bound to loopback"
        );
        assert!(text.contains("potluck-data:/data"), "{name}: volume");
        assert!(text.contains("POTLUCK_PUBLIC_URL"), "{name}: public url");
        assert!(text.contains("health-check"), "{name}: health check");
        for secret in ["TOKEN", "PASSWORD", "SECRET", "OIDC_CLIENT_SECRET"] {
            assert!(
                !text.to_uppercase().contains(secret),
                "{name}: carries {secret}"
            );
        }
    }
}

#[test]
fn compose_hardens_the_container() {
    let text = compose();
    for needle in [
        "read_only: true",
        "no-new-privileges:true",
        "- ALL",
        "- /tmp",
        "restart: unless-stopped",
    ] {
        assert!(text.contains(needle), "compose lacks {needle:?}");
    }
    assert!(
        text.contains("${POTLUCK_PUBLIC_URL:?"),
        "the URL is required, not defaulted"
    );
}

#[test]
fn quadlet_hardens_the_container_and_restarts_on_failure() {
    let text = quadlet();
    for needle in [
        "[Container]",
        "ReadOnly=true",
        "NoNewPrivileges=true",
        "DropCapability=ALL",
        "Tmpfs=/tmp",
        "HealthCmd=potluck health-check",
        "HealthOnFailure=kill",
        "AutoUpdate=registry",
        "Restart=always",
        "WantedBy=default.target",
    ] {
        assert!(text.contains(needle), "quadlet lacks {needle:?}");
    }
}

/// `compose config -q` parses and validates without a daemon; skipped when no tool exists.
#[test]
fn compose_validates_with_an_installed_compose_tool() {
    let file = repo_root().join("compose.yaml");
    let candidates: [(&str, &[&str]); 2] = [("docker", &["compose"]), ("podman", &["compose"])];
    for (bin, sub) in candidates {
        let out = Command::new(bin)
            .args(sub)
            .arg("-f")
            .arg(&file)
            .args(["config", "-q"])
            .env("POTLUCK_PUBLIC_URL", "https://potluck.example.com")
            .output();
        let Ok(out) = out else { continue };
        if out.status.code() == Some(125)
            || String::from_utf8_lossy(&out.stderr).contains("is not a docker command")
        {
            continue; // the engine exists but has no compose provider
        }
        assert!(
            out.status.success(),
            "{bin} compose config: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    eprintln!("skipped: no compose tool installed");
}

/// The Quadlet generator's dry run, confined to a temp copy of the unit, must emit the
/// `podman run` line with the loopback port, the volume and the health command.
#[test]
fn quadlet_generates_a_service_in_a_dry_run() {
    let generator = [
        "/usr/lib/systemd/system-generators/podman-system-generator",
        "/usr/lib/podman/quadlet",
        "/usr/libexec/podman/quadlet",
    ]
    .into_iter()
    .find(|p| exists(p));
    let Some(generator) = generator else {
        eprintln!("skipped: no Quadlet generator installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    fs::copy(
        repo_root().join("deploy/potluck.container"),
        dir.path().join("potluck.container"),
    )
    .unwrap();
    let out = Command::new(generator)
        .args(["-dryrun", "-user"])
        .env("QUADLET_UNIT_DIRS", dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "dry run failed: {}{stdout}",
        String::from_utf8_lossy(&out.stderr)
    );
    let exec = stdout
        .lines()
        .find(|l| l.starts_with("ExecStart="))
        .expect("ExecStart line");
    for needle in [
        "--publish 127.0.0.1:8080:8080",
        "-v potluck-data:/data",
        "--read-only",
        "--cap-drop all",
        "health-check",
        IMAGE,
    ] {
        assert!(exec.contains(needle), "ExecStart lacks {needle:?}: {exec}");
    }
}
