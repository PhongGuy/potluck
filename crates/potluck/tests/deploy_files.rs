//! compose.yaml and deploy/potluck.container: the invariants both share, a compose
//! validation when a compose tool is installed, and a Quadlet dry run when podman's
//! generator is installed, and the smoke script against a stub engine. Nothing here starts a
//! container or touches user config.

use std::fs;
use std::os::unix::fs::PermissionsExt;
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
        "name: potluck-data",
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
        // The same timeout and retries as the image's HEALTHCHECK and compose.yaml.
        "HealthTimeout=5s",
        "HealthRetries=3",
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

#[test]
fn the_readme_and_the_notices_name_the_image_the_proxy_and_the_git_licence() {
    let readme = fs::read_to_string(repo_root().join("README.md")).unwrap();
    let headings: Vec<&str> = readme.lines().filter(|l| l.starts_with("## ")).collect();
    assert_eq!(
        headings,
        [
            "## Run it",
            "## First admin",
            "## Reverse proxy and TLS",
            "## Updating",
            "## Backups",
            "## Verifying the image",
            "## Bind mounts",
            "## License",
        ]
    );
    for needle in [
        IMAGE,
        "127.0.0.1:8080:8080",
        "POTLUCK_PUBLIC_URL",
        "reverse_proxy 127.0.0.1:8080",
        "proxy_pass http://127.0.0.1:8080",
        "client_max_body_size 110m",
        "proxy_buffering off",
        "gh attestation verify oci://ghcr.io/phongguy/potluck:",
        "repository is public",
        "podman unshare chown 65532:65532",
        "THIRD_PARTY_NOTICES.md",
        "login ghcr.io",
        "POTLUCK_TRUSTED_PROXIES",
        "debian:trixie-slim tar -C /data -xf /backup/potluck-data.tar",
        "podman exec potluck potluck admin user create alice --server-admin",
        "docker exec potluck potluck admin user create alice --server-admin",
        "--password-stdin",
    ] {
        assert!(readme.contains(needle), "README lacks {needle:?}");
    }
    let notices = fs::read_to_string(repo_root().join("THIRD_PARTY_NOTICES.md")).unwrap();
    for needle in [
        "GPL-2.0",
        "deb.debian.org/debian/pool/main/g/git/",
        "apt-get source git",
        "dpkg-query",
        "MIT OR Apache-2.0",
        "git and its dependencies",
        "https://snapshot.debian.org/package/git/",
    ] {
        assert!(notices.contains(needle), "notices lack {needle:?}");
    }
}

/// An engine stub that logs each call's arguments and claims the image has a HEALTHCHECK.
fn stub_engine(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("engine");
    fs::write(
        &path,
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/calls\"\n\
         if [[ $1 == image ]]; then echo '[CMD potluck health-check]'; fi\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn smoke_script(dir: &Path, mode: &str, image: &str) -> (std::process::Output, String) {
    let out = Command::new(repo_root().join("scripts/container-smoke.sh"))
        .arg(mode)
        .env("ENGINE", stub_engine(dir))
        .env("IMAGE", image)
        .output()
        .unwrap();
    let calls = fs::read_to_string(dir.join("calls")).unwrap_or_default();
    (out, calls)
}

/// The stub answers no port, so the script stops after starting the container.
#[test]
fn the_smoke_run_is_hardened_like_the_deploy_files() {
    let dir = tempfile::tempdir().unwrap();
    let (_, calls) = smoke_script(dir.path(), "smoke", "localhost/potluck:stub");
    let run = calls
        .lines()
        .find(|l| l.starts_with("run -d"))
        .unwrap_or_else(|| panic!("no run call: {calls}"));
    for needle in [
        "--read-only",
        "--tmpfs /tmp",
        "--cap-drop ALL",
        "--security-opt no-new-privileges",
    ] {
        assert!(run.contains(needle), "smoke run lacks {needle:?}: {run}");
    }
}

#[test]
fn build_mode_tags_the_given_image_and_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let (out, calls) = smoke_script(dir.path(), "build", "localhost/potluck:stub-build");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        calls
            .lines()
            .any(|l| l.starts_with("build ") && l.contains("-t localhost/potluck:stub-build")),
        "{calls}"
    );
    assert!(
        !calls.contains("rmi"),
        "build mode removed its image: {calls}"
    );
}
