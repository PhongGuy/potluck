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
            "## Status",
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
        "POTLUCK_TRUSTED_PROXIES",
        "debian:trixie-slim tar -C /data -xf /backup/potluck-data.tar",
        "podman exec potluck potluck admin user create alice --server-admin",
        "docker exec potluck potluck admin user create alice --server-admin",
        "--password-stdin",
        "not ready for use yet",
        "https://github.com/PhongGuy/apinata",
        "https://github.com/PhongGuy/dbento",
        "podman build -t ghcr.io/phongguy/potluck:latest .",
    ] {
        assert!(readme.contains(needle), "README lacks {needle:?}");
    }
    assert!(
        !readme.contains("login ghcr.io"),
        "the image is public, so the README asks for no registry login"
    );
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

const PODMAN_CLEAR: &str =
    "podman run --rm -v potluck-data:/data debian:trixie-slim find /data -mindepth 1 -delete";
const DOCKER_CLEAR: &str =
    "docker run --rm -v potluck-data:/data debian:trixie-slim find /data -mindepth 1 -delete";
const PODMAN_IMPORT: &str = "podman volume import potluck-data potluck-data.tar";
const DOCKER_EXTRACT: &str = "debian:trixie-slim tar -C /data -xf /backup/potluck-data.tar";

/// The bodies of the fenced code blocks in the README's `## Backups` section.
fn backup_blocks(readme: &str) -> Vec<&str> {
    let start = readme.find("## Backups").expect("Backups section");
    let rest = &readme[start..];
    let section = &rest[..rest.find("\n## ").unwrap_or(rest.len())];
    section.split("```").skip(1).step_by(2).collect()
}

/// True when every needle occurs in `text`, each after the end of the one before it.
fn in_order(text: &str, needles: &[&str]) -> bool {
    let mut from = 0;
    for needle in needles {
        match text[from..].find(needle) {
            Some(at) => from += at + needle.len(),
            None => return false,
        }
    }
    true
}

/// Import and `tar -x` merge into what the volume holds, so a stale `server.db-wal` would be
/// replayed over the restored database; every restore must empty the volume first.
#[test]
fn the_readme_restores_only_into_an_emptied_volume() {
    let readme = fs::read_to_string(repo_root().join("README.md")).unwrap();
    let blocks = backup_blocks(&readme);
    let restores: Vec<&str> = blocks
        .iter()
        .copied()
        .filter(|b| b.contains(PODMAN_IMPORT) || b.contains(DOCKER_EXTRACT))
        .collect();
    assert_eq!(
        restores.len(),
        3,
        "one restore block each for podman, docker and Quadlet: {blocks:#?}"
    );
    for block in &restores {
        let (clear, write) = if block.contains(PODMAN_IMPORT) {
            (PODMAN_CLEAR, PODMAN_IMPORT)
        } else {
            (DOCKER_CLEAR, DOCKER_EXTRACT)
        };
        assert!(
            in_order(block, &[clear, write]),
            "restore writes before emptying the volume: {block}"
        );
    }
    for sequence in [
        [
            "podman stop potluck",
            PODMAN_CLEAR,
            PODMAN_IMPORT,
            "podman start potluck",
        ],
        [
            "docker stop potluck",
            DOCKER_CLEAR,
            DOCKER_EXTRACT,
            "docker start potluck",
        ],
        [
            "systemctl --user stop potluck",
            PODMAN_CLEAR,
            PODMAN_IMPORT,
            "systemctl --user start potluck",
        ],
    ] {
        assert!(
            restores.iter().any(|b| in_order(b, &sequence)),
            "no restore block runs {sequence:?}"
        );
    }
}

fn containerfile() -> String {
    fs::read_to_string(repo_root().join("Containerfile")).unwrap()
}

/// tini is PID 1 and reaps orphans for every way of running the image, so the deploy files
/// add no second init of their own.
#[test]
fn the_image_runs_tini_as_pid_1_in_front_of_potluck() {
    let text = containerfile();
    let install = text
        .lines()
        .find(|l| l.contains("apt-get install"))
        .expect("an apt-get install line");
    assert!(
        install.split_whitespace().any(|w| w == "tini"),
        "tini is not installed: {install}"
    );
    assert!(
        text.contains(r#"ENTRYPOINT ["tini", "-s", "--", "potluck"]"#),
        "the entrypoint does not run potluck under tini"
    );
    assert!(
        text.contains(r#"CMD ["serve"]"#),
        "serve is no longer the default"
    );
    assert!(
        !compose().contains("init: true"),
        "compose adds a second init"
    );
    assert!(
        !quadlet().contains("RunInit="),
        "the Quadlet unit adds a second init"
    );
}

#[test]
fn the_smoke_test_checks_reaping_and_a_graceful_stop() {
    let script = fs::read_to_string(repo_root().join("scripts/container-smoke.sh")).unwrap();
    for needle in ["/proc/1/comm", "zombie", "stop -t 20"] {
        assert!(script.contains(needle), "smoke script lacks {needle:?}");
    }
}
