//! The shape of release.yml that the generic workflow invariants do not pin.

use std::fs;

use crate::common::{jobs, repo_root};

pub fn release() -> Vec<(String, String)> {
    jobs(&fs::read_to_string(repo_root().join(".github/workflows/release.yml")).unwrap())
}

/// The steps of `job`, each from its `- ` line up to the next step.
fn steps(job: &str) -> Vec<String> {
    let mut steps: Vec<String> = Vec::new();
    let mut in_steps = false;
    for line in job.lines() {
        if line == "    steps:" {
            in_steps = true;
            continue;
        }
        if !in_steps {
            continue;
        }
        if line.starts_with("      - ") {
            steps.push(String::new());
        }
        if let Some(step) = steps.last_mut() {
            step.push_str(line);
            step.push('\n');
        }
    }
    steps
}

/// The one step of `job` that contains `needle`.
fn step(job: &str, needle: &str) -> String {
    let mut found: Vec<String> = steps(job)
        .into_iter()
        .filter(|s| s.contains(needle))
        .collect();
    assert_eq!(found.len(), 1, "steps holding {needle:?}: {found:?}");
    found.remove(0)
}

pub fn job<'a>(jobs: &'a [(String, String)], name: &str) -> &'a str {
    &jobs
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no job {name}"))
        .1
}

#[test]
fn runs_on_version_tags_with_a_dry_run_dispatch() {
    let text = fs::read_to_string(repo_root().join(".github/workflows/release.yml")).unwrap();
    assert!(text.contains("tags: [\"v*\"]"), "{text}");
    assert!(text.contains("dry_run:"), "{text}");
    assert!(text.contains("cancel-in-progress: false"), "{text}");
}

#[test]
fn build_runs_natively_on_both_architectures_smokes_and_pushes_by_digest() {
    let jobs = release();
    let build = job(&jobs, "build");
    for needle in [
        "runner: ubuntu-26.04 }",
        "runner: ubuntu-26.04-arm }",
        "scripts/container-smoke.sh smoke",
        "push-by-digest=true",
        "sbom: true",
        "provenance: mode=max",
        "packages: write",
        "if-no-files-found: error",
        "docker pull --platform \"linux/$ARCH\" \"$IMAGE@$DIGEST\"",
        ".Config.Healthcheck.Test",
        ".Config.User == \"65532:65532\"",
    ] {
        assert!(build.contains(needle), "build lacks {needle:?}");
    }
}

#[test]
fn merge_tags_the_version_moves_latest_only_on_a_final_release_and_annotates() {
    let jobs = release();
    let merge = job(&jobs, "merge");
    for needle in [
        "imagetools create",
        "tags=(-t \"$IMAGE:$REF_NAME\")",
        "if [[ $REF_NAME =~ ^v[0-9]+\\.[0-9]+\\.[0-9]+$ ]]; then tags+=(-t \"$IMAGE:latest\"); fi",
        "index:org.opencontainers.image.source=",
        "digest: ${{ steps.index.outputs.digest }}",
        // The index digest step fails closed: no empty or malformed digest reaches manifest, sign or draft.
        "set -euo pipefail",
        "jq -re '.digest | select(test(\"^sha256:[0-9a-f]{64}$\"))'",
    ] {
        assert!(merge.contains(needle), "merge lacks {needle:?}");
    }
    for forbidden in ["Healthcheck", "scripts/"] {
        assert!(
            !merge.contains(forbidden),
            "merge asserts after tagging: {forbidden:?}"
        );
    }
}

#[test]
fn the_five_jobs_run_in_order_and_sign_is_the_release_job() {
    let jobs = release();
    let names: Vec<&str> = jobs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["build", "merge", "manifest", "sign", "draft"]);
    let sign = job(&jobs, "sign");
    for needle in [
        "needs: [merge, manifest]",
        "if: ${{ startsWith(github.ref, 'refs/tags/v') && !inputs.dry_run }}",
        "environment: release",
        "id-token: write",
        "attestations: write",
        "artifact-metadata: write",
        "packages: write",
        "subject-name: ${{ env.IMAGE }}",
        "subject-digest: ${{ needs.merge.outputs.digest }}",
        "actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2",
        "minisign=0.12-1build1",
        "name: manifest-unsigned",
        "name: manifest-signed",
    ] {
        assert!(sign.contains(needle), "sign lacks {needle:?}");
    }
    for forbidden in ["actions/checkout", "scripts/"] {
        assert!(!sign.contains(forbidden), "sign runs {forbidden:?}");
    }
    assert_eq!(
        jobs.iter()
            .filter(|(_, b)| b.contains("environment: release"))
            .count(),
        1
    );
}

#[test]
fn draft_writes_notes_with_the_script_attaches_the_signed_manifest_and_only_drafts() {
    let jobs = release();
    let draft = job(&jobs, "draft");
    for needle in [
        "needs: [merge, sign]",
        "if: ${{ startsWith(github.ref, 'refs/tags/v') && !inputs.dry_run }}",
        "ATTESTED: ${{ needs.sign.outputs.attested }}",
        "fetch-depth: 0",
        "[[ $DIGEST =~ ^sha256:[0-9a-f]{64}$ ]]",
        "scripts/release.sh notes",
        "--draft --verify-tag",
        "contents: write",
        "name: manifest-signed",
        "jq -e --arg v \"$version\" --arg d \"$DIGEST\" '.version == $v and .digest == $d' \"$RUNNER_TEMP/release/potluck-release.json\"",
        "\"$RUNNER_TEMP/release/potluck-release.json\" \"$RUNNER_TEMP/release/potluck-release.json.sig\"",
    ] {
        assert!(draft.contains(needle), "draft lacks {needle:?}");
    }
    // Without always(), a failed or skipped sign leaves no draft behind it.
    assert!(
        !draft.contains("always()"),
        "draft runs after a failed sign"
    );
}

#[test]
fn draft_marks_a_pre_release_tag_as_a_prerelease() {
    let jobs = release();
    let draft = job(&jobs, "draft");
    for needle in [
        "flags=(--draft --verify-tag)",
        "if [[ $REF_NAME == *-* ]]; then flags+=(--prerelease); fi",
        "\"${flags[@]}\"",
    ] {
        assert!(draft.contains(needle), "draft lacks {needle:?}");
    }
}

#[test]
fn build_refuses_a_bad_tag_before_it_logs_in_or_pushes() {
    let jobs = release();
    let build = job(&jobs, "build");
    let check =
        "if [[ $PUBLISH == true ]]; then scripts/release.sh version \"$REF_NAME\" > /dev/null; fi";
    let at = build
        .find(check)
        .unwrap_or_else(|| panic!("build lacks {check:?}"));
    for later in ["docker/login-action", "push-by-digest=true"] {
        assert!(
            at < build.find(later).unwrap(),
            "the tag check comes after {later}"
        );
    }
}

#[test]
fn build_checks_the_tag_against_the_workspace_version_before_it_logs_in_or_pushes() {
    let jobs = release();
    let build = job(&jobs, "build");
    let check =
        "if [[ $PUBLISH == true ]]; then scripts/release.sh check-version \"$REF_NAME\"; fi";
    let at = build
        .find(check)
        .unwrap_or_else(|| panic!("build lacks {check:?}"));
    for later in ["docker/login-action", "push-by-digest=true"] {
        assert!(
            at < build.find(later).unwrap(),
            "the version check comes after {later}"
        );
    }
}

#[test]
fn manifest_writes_the_unsigned_file_from_the_merged_digest() {
    let jobs = release();
    let manifest = job(&jobs, "manifest");
    for needle in [
        "needs: merge",
        "if: ${{ startsWith(github.ref, 'refs/tags/v') && !inputs.dry_run }}",
        "persist-credentials: false",
        "DIGEST: ${{ needs.merge.outputs.digest }}",
        "scripts/release.sh manifest \"$REF_NAME\" \"$DIGEST\" \"$RUNNER_TEMP/release/potluck-release.json\"",
        "name: manifest-unsigned",
        "if-no-files-found: error",
    ] {
        assert!(manifest.contains(needle), "manifest lacks {needle:?}");
    }
    // Line-shaped: the next job's leading comment is part of this body.
    for forbidden in ["environment:", "permissions:"] {
        assert!(
            !manifest
                .lines()
                .any(|l| l.trim_start().starts_with(forbidden)),
            "manifest declares {forbidden:?}"
        );
    }
    assert!(!manifest.contains("secrets."), "manifest reads a secret");
}

#[test]
fn sign_reads_contents_and_buildx_downloads_no_cached_binary() {
    let jobs = release();
    assert!(
        job(&jobs, "sign").contains("contents: read"),
        "sign lacks contents: read"
    );
    let text = fs::read_to_string(repo_root().join(".github/workflows/release.yml")).unwrap();
    assert_eq!(
        text.matches("docker/setup-buildx-action@").count(),
        text.matches("cache-binary: false").count(),
        "every setup-buildx sets cache-binary: false"
    );
}

#[test]
fn sign_attests_only_a_public_repository_but_always_signs() {
    let jobs = release();
    let sign = job(&jobs, "sign");
    let public = "if: ${{ !github.event.repository.private }}";
    assert!(
        !sign
            .lines()
            .any(|l| l.starts_with("    if:") && l.contains("private")),
        "a private repository would skip signing"
    );
    for uses in ["docker/login-action@", "actions/attest@"] {
        assert!(
            step(sign, uses).contains(public),
            "{uses} runs in a private repository"
        );
    }
    for unconditional in [
        "minisign=0.12-1build1",
        "minisign -S",
        "name: manifest-signed",
    ] {
        assert!(
            !step(sign, unconditional).contains("if:"),
            "{unconditional} is conditional"
        );
    }
    assert!(step(sign, "actions/attest@").contains("id: attest"));
    assert!(sign.contains("attested: ${{ steps.attest.outcome == 'success' }}"));
}

#[test]
fn the_signing_secrets_reach_minisign_only_through_env_and_pipes() {
    let text = fs::read_to_string(repo_root().join(".github/workflows/release.yml")).unwrap();
    let secrets: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("secrets.") && !l.contains("secrets.GITHUB_TOKEN"))
        .map(str::trim)
        .collect();
    assert_eq!(
        secrets,
        [
            "CARGO_PACKAGER_SIGN_PRIVATE_KEY: ${{ secrets.CARGO_PACKAGER_SIGN_PRIVATE_KEY }}",
            "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD: ${{ secrets.CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD }}",
        ]
    );
    let jobs = release();
    for (name, body) in &jobs {
        assert!(
            name == "sign" || !body.contains("CARGO_PACKAGER_SIGN"),
            "job {name} sees the signing key"
        );
    }
    let signing = step(job(&jobs, "sign"), "minisign -S");
    assert!(
        signing.contains("CARGO_PACKAGER_SIGN_PRIVATE_KEY: ${{"),
        "{signing}"
    );
    let run = &signing[signing.find("run: |").expect("no run block")..];
    assert!(
        !run.contains("${{"),
        "the signing script interpolates an expression: {run}"
    );
    for needle in [
        "shell: bash",
        "printf '%s\\n' \"$CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD\" |",
        "minisign -S -s <(printf '%s' \"$CARGO_PACKAGER_SIGN_PRIVATE_KEY\" | base64 -d)",
        "-m potluck-release.json -x potluck-release.json.minisig",
        "base64 -w0 potluck-release.json.minisig > potluck-release.json.sig",
    ] {
        assert!(
            signing.contains(needle),
            "the signing step lacks {needle:?}"
        );
    }
    // Each secret is read once, by a builtin, so neither reaches argv, a file or the log.
    assert_eq!(
        run.matches("$CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD")
            .count(),
        1
    );
    assert_eq!(
        run.matches("\"$CARGO_PACKAGER_SIGN_PRIVATE_KEY\"").count(),
        1
    );
}
