//! The hardening invariants every workflow under .github/workflows keeps: SHA-pinned actions
//! with a version comment, a top-level `permissions:` that per-job blocks only widen, `timeout-minutes` on every job,
//! credential-free checkouts, and `environment: release` on at most one job that builds nothing.

use std::fs;
use std::path::PathBuf;

use crate::common::{jobs, repo_root};

fn workflows() -> Vec<(PathBuf, String)> {
    let dir = repo_root().join(".github/workflows");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "yml"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no workflows under {}", dir.display());
    files
        .into_iter()
        .map(|p| {
            let text = fs::read_to_string(&p).unwrap();
            (p, text)
        })
        .collect()
}

#[test]
fn ci_declares_its_jobs() {
    let text = fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).unwrap();
    let names: Vec<String> = jobs(&text).into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        names,
        ["gate", "rules", "advisories", "zizmor", "container"]
    );
    // The advisories job runs a checksum-pinned cargo-deny against the lockfile.
    let (_, advisories) = &jobs(&text)[2];
    for needle in [
        "CARGO_DENY_VERSION: 0.20.2",
        "sha256sum -c",
        "--locked check advisories",
    ] {
        assert!(advisories.contains(needle), "{needle} in {advisories}");
    }
    let deny = fs::read_to_string(repo_root().join("deny.toml")).unwrap();
    assert!(
        deny.contains("RUSTSEC-2023-0071") && deny.contains("reason"),
        "{deny}"
    );
    let (_, container) = &jobs(&text)[4];
    assert!(container.contains("runs-on: ubuntu-26.04\n"), "{container}");
    assert!(
        container.contains("scripts/container-smoke.sh"),
        "{container}"
    );
}

#[test]
fn the_gate_checks_the_public_api_after_the_tests() {
    let text = fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).unwrap();
    let (name, gate) = &jobs(&text)[0];
    assert_eq!(name, "gate");
    let at = |needle: &str| {
        gate.find(needle)
            .unwrap_or_else(|| panic!("{needle} in {gate}"))
    };
    let test = at("cargo test --workspace --locked");
    let json = at(
        "RUSTC_BOOTSTRAP=1 cargo rustdoc -p potluck --lib --locked -- -Z unstable-options --output-format json",
    );
    let check = at("scripts/check-public-api.sh target/doc/potluck.json");
    assert!(
        test < json && json < check,
        "test, JSON, check in order: {gate}"
    );
    // The unstable switch stays on the rustdoc line; as an env key it would reach every build.
    assert!(
        !text.contains("RUSTC_BOOTSTRAP:"),
        "ci.yml sets RUSTC_BOOTSTRAP as an env key"
    );
}

#[test]
fn the_rules_job_tests_pinned_rules_crates_and_the_gate_checks_potluck_kinds_api() {
    let text = fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).unwrap();
    let (name, rules) = &jobs(&text)[1];
    assert_eq!(name, "rules");
    assert!(
        rules.contains("scripts/rules-crates.sh test Cargo.lock"),
        "{rules}"
    );
    let (_, gate) = &jobs(&text)[0];
    assert!(
        gate.contains("cargo rustdoc -p potluck-kind --lib --locked"),
        "{gate}"
    );
    assert!(
        gate.contains("scripts/check-public-api.sh target/doc/potluck_kind.json"),
        "{gate}"
    );
}

#[test]
fn every_action_is_pinned_to_a_full_sha_with_a_version_comment() {
    for (path, text) in workflows() {
        for line in text.lines().filter(|l| {
            l.trim_start().starts_with("- uses:") || l.trim_start().starts_with("uses:")
        }) {
            let (_, rest) = line.split_once("uses:").unwrap();
            let (_, pinned) = rest
                .trim()
                .split_once('@')
                .unwrap_or_else(|| panic!("{}: unpinned {line}", path.display()));
            let (sha, comment) = pinned
                .split_once('#')
                .unwrap_or_else(|| panic!("{}: no version comment on {line}", path.display()));
            let sha = sha.trim();
            assert!(
                sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
                "{}: not a 40-hex SHA: {line}",
                path.display()
            );
            assert!(
                comment.trim().starts_with('v'),
                "{}: comment is not a version: {line}",
                path.display()
            );
        }
    }
}

#[test]
fn every_workflow_starts_read_only_and_every_job_has_a_timeout() {
    for (path, text) in workflows() {
        let top = text
            .lines()
            .position(|l| l == "permissions:")
            .unwrap_or_else(|| panic!("{}: no top-level permissions", path.display()));
        assert_eq!(
            text.lines().nth(top + 1).map(str::trim),
            Some("contents: read"),
            "{}: top level is not contents: read",
            path.display()
        );
        let jobs = jobs(&text);
        assert!(!jobs.is_empty(), "{}: no jobs parsed", path.display());
        for (name, body) in jobs {
            assert!(
                body.contains("timeout-minutes:"),
                "{}: job {name} has no timeout",
                path.display()
            );
            // A per-job block exists only to widen: it must grant something beyond the top level.
            if let Some(start) = body.find("    permissions:\n") {
                let block: Vec<&str> = body[start..]
                    .lines()
                    .skip(1)
                    .take_while(|l| l.starts_with("      "))
                    .collect();
                assert!(
                    !block.is_empty(),
                    "{}: job {name} has an empty permissions block",
                    path.display()
                );
                assert!(
                    block
                        .iter()
                        .any(|l| !l.trim_start().starts_with("contents: read")),
                    "{}: job {name} declares permissions but widens nothing",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn every_checkout_drops_its_credentials() {
    for (path, text) in workflows() {
        let checkouts = text.matches("actions/checkout@").count();
        let dropped = text.matches("persist-credentials: false").count();
        assert_eq!(
            checkouts,
            dropped,
            "{}: a checkout keeps its token",
            path.display()
        );
    }
}

#[test]
fn the_release_environment_binds_one_job_that_builds_nothing() {
    for (path, text) in workflows() {
        let bound: Vec<(String, String)> = jobs(&text)
            .into_iter()
            .filter(|(_, body)| body.contains("environment: release"))
            .collect();
        assert!(
            bound.len() <= 1,
            "{}: several jobs bind the release environment",
            path.display()
        );
        for (name, body) in bound {
            for forbidden in [
                "cargo ",
                "build-push-action",
                "podman build",
                "docker build",
                "actions/checkout",
            ] {
                assert!(
                    !body.contains(forbidden),
                    "{}: job {name} runs {forbidden:?} with the release environment",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn release_jobs_restore_no_caches() {
    let path = repo_root().join(".github/workflows/release.yml");
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    for forbidden in ["rust-cache", "actions/cache", "cache:", "cache-from"] {
        assert!(
            !text.contains(forbidden),
            "release.yml restores a cache: {forbidden}"
        );
    }
}

#[test]
fn release_publishes_only_from_version_tags() {
    // A dispatch on any other tag must not push an image, so every publishing gate names the
    // v prefix rather than the ref type.
    let text = fs::read_to_string(repo_root().join(".github/workflows/release.yml")).unwrap();
    assert!(
        !text.contains("github.ref_type"),
        "release.yml gates on the ref type, which any tag passes"
    );
    let gate = "startsWith(github.ref, 'refs/tags/v')";
    assert!(
        text.lines()
            .any(|l| l.trim_start().starts_with("PUBLISH:") && l.contains(gate)),
        "PUBLISH does not require a v tag"
    );
    for (name, body) in jobs(&text).into_iter().filter(|(n, _)| n != "build") {
        assert!(
            body.lines()
                .any(|l| l.trim_start().starts_with("if:") && l.contains(gate)),
            "job {name} does not require a v tag"
        );
    }
}
