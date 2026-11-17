//! scripts/rules-crates.sh over lockfile texts: what it lists, what lock shapes it refuses, and
//! that with nothing pinned the test step is a no-op.

use std::process::{Command, Output};

use crate::common::repo_root;

const KIND_PATH: &str = "[[package]]\nname = \"potluck-kind\"\nversion = \"0.1.0\"\n\n";
const APINATA: &str = "[[package]]\nname = \"apinata-rules\"\nversion = \"0.1.0\"\nsource = \"git+https://github.com/PhongGuy/apinata?tag=apinata-rules-v0.1.0#0123456789abcdef0123456789abcdef01234567\"\ndependencies = [\n \"potluck-kind\",\n]\n\n";
const SERDE: &str = "[[package]]\nname = \"serde\"\nversion = \"1.0.229\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n";

fn run(mode: &str, lock: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Cargo.lock");
    std::fs::write(&path, format!("version = 4\n\n{lock}")).unwrap();
    Command::new(repo_root().join("scripts/rules-crates.sh"))
        .args([mode, path.to_str().unwrap()])
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn list_names_each_pinned_rules_crate() {
    let out = run("list", &format!("{KIND_PATH}{APINATA}{SERDE}"));
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        stdout(&out),
        "apinata-rules https://github.com/PhongGuy/apinata apinata-rules-v0.1.0 0123456789abcdef0123456789abcdef01234567\n"
    );
    assert_eq!(stdout(&run("list", &format!("{KIND_PATH}{SERDE}"))), "");
}

#[test]
fn check_lock_wants_one_source_less_potluck_kind() {
    assert_eq!(
        run("check-lock", &format!("{KIND_PATH}{APINATA}"))
            .status
            .code(),
        Some(0)
    );
    let git_kind = "[[package]]\nname = \"potluck-kind\"\nversion = \"0.1.0\"\nsource = \"git+https://github.com/PhongGuy/potluck?tag=potluck-kind-v0.1.0#0123456789abcdef0123456789abcdef01234567\"\n\n";
    for bad in [
        format!("{KIND_PATH}{git_kind}"),
        git_kind.to_owned(),
        SERDE.to_owned(),
    ] {
        let out = run("check-lock", &bad);
        assert_eq!(out.status.code(), Some(1), "{bad}");
        assert!(
            !String::from_utf8_lossy(&out.stderr).trim().is_empty()
                || !stdout(&out).trim().is_empty()
        );
    }
}

#[test]
fn a_rules_crate_pinned_any_other_way_fails_list() {
    let rev = APINATA.replace(
        "?tag=apinata-rules-v0.1.0",
        "?rev=0123456789abcdef0123456789abcdef01234567",
    );
    let path = "[[package]]\nname = \"dbento-rules\"\nversion = \"0.1.0\"\n\n";
    for lock in [format!("{KIND_PATH}{rev}"), format!("{KIND_PATH}{path}")] {
        assert_eq!(run("list", &lock).status.code(), Some(1), "{lock}");
    }
}

#[test]
fn test_with_nothing_pinned_is_a_no_op() {
    let out = run("test", &format!("{KIND_PATH}{SERDE}"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out), "rules: no rules crate pinned\n");
}

#[test]
fn the_real_lock_passes_check_lock() {
    let out = Command::new(repo_root().join("scripts/rules-crates.sh"))
        .args([
            "check-lock",
            repo_root().join("Cargo.lock").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

const DBENTO: &str = "[[package]]\nname = \"dbento-rules\"\nversion = \"0.1.0\"\nsource = \"git+https://github.com/PhongGuy/dbento?tag=dbento-rules-v0.1.0#0123456789abcdef0123456789abcdef01234567\"\n\n";

/// What a stubbed `test` run did: its output, each cargo and git call's arguments one line per
/// call, and what is left in its TMPDIR.
struct Stubbed {
    out: Output,
    cargo: String,
    git: String,
    left: Vec<std::path::PathBuf>,
}

/// Runs `test` mode over a lock with stub `git` and `cargo` first on PATH, so no network is
/// reached; `switches` are env vars that steer the stubs (see the stub bodies).
fn run_stubbed(lock: &str, switches: &[(&str, &str)]) -> Stubbed {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (bin, tmp) = (dir.path().join("bin"), dir.path().join("tmp"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    // The stub clone creates its target dir; a clone's HEAD is the pinned commit unless
    // STUB_HEAD says otherwise.
    let git = "#!/bin/sh\necho \"$*\" >> \"$STUB_GIT_LOG\"\nif [ \"$1\" = clone ]; then for a; do last=$a; done; mkdir -p \"$last\"; exit 0; fi\nif [ \"$1\" = -C ]; then echo \"${STUB_HEAD:-0123456789abcdef0123456789abcdef01234567}\"; exit 0; fi\nexit 2\n";
    let cargo = "#!/bin/sh\nif [ -n \"$STUB_READ_STDIN\" ]; then cat > /dev/null; fi\necho \"$*\" >> \"$STUB_LOG\"\nif [ -n \"$STUB_CARGO_FAIL\" ]; then exit 3; fi\n";
    for (name, body) in [("git", git), ("cargo", cargo)] {
        let path = bin.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let lock_path = dir.path().join("Cargo.lock");
    std::fs::write(&lock_path, format!("version = 4\n\n{lock}")).unwrap();
    let (cargo_log, git_log) = (dir.path().join("cargo.log"), dir.path().join("git.log"));
    let mut cmd = Command::new(repo_root().join("scripts/rules-crates.sh"));
    cmd.args(["test", lock_path.to_str().unwrap()])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("TMPDIR", &tmp)
        .env("STUB_LOG", &cargo_log)
        .env("STUB_GIT_LOG", &git_log)
        .env_remove("STUB_READ_STDIN")
        .env_remove("STUB_HEAD")
        .env_remove("STUB_CARGO_FAIL")
        .envs(switches.iter().copied());
    let out = cmd.output().unwrap();
    Stubbed {
        out,
        cargo: std::fs::read_to_string(&cargo_log).unwrap_or_default(),
        git: std::fs::read_to_string(&git_log).unwrap_or_default(),
        left: std::fs::read_dir(&tmp)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect(),
    }
}

const BOTH_TESTED: &str = "test -p apinata-rules --locked\ntest -p dbento-rules --locked\n";

#[test]
fn test_runs_every_pinned_crate_and_removes_its_clones() {
    let run = run_stubbed(&format!("{KIND_PATH}{APINATA}{DBENTO}"), &[]);
    assert_eq!(
        run.out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&run.out.stderr)
    );
    assert_eq!(run.cargo, BOTH_TESTED);
    assert!(run.left.is_empty(), "{:?}", run.left);
}

#[test]
fn a_crate_test_reading_stdin_does_not_skip_the_next_crate() {
    let run = run_stubbed(
        &format!("{KIND_PATH}{APINATA}{DBENTO}"),
        &[("STUB_READ_STDIN", "1")],
    );
    assert_eq!(
        run.out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&run.out.stderr)
    );
    assert_eq!(run.cargo, BOTH_TESTED);
}

#[test]
fn test_clones_each_crate_at_its_pinned_tag_and_checks_its_head() {
    let run = run_stubbed(&format!("{KIND_PATH}{APINATA}{DBENTO}"), &[]);
    assert_eq!(
        run.out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&run.out.stderr)
    );
    let calls: Vec<&str> = run.git.lines().collect();
    assert_eq!(calls.len(), 4, "{calls:?}");
    let pins = [
        (
            "apinata-rules",
            "https://github.com/PhongGuy/apinata",
            "apinata-rules-v0.1.0",
        ),
        (
            "dbento-rules",
            "https://github.com/PhongGuy/dbento",
            "dbento-rules-v0.1.0",
        ),
    ];
    for (pair, (name, url, tag)) in calls.chunks(2).zip(pins) {
        let clone = format!("clone --quiet --depth 1 --branch {tag} -- {url} ");
        assert!(
            pair[0].starts_with(&clone) && pair[0].ends_with(&format!("/{name}")),
            "{pair:?}"
        );
        let dest = &pair[0][clone.len()..];
        assert_eq!(pair[1], format!("-C {dest} rev-parse HEAD"), "{pair:?}");
    }
}

#[test]
fn a_tag_moved_since_the_pin_fails_before_any_crate_test_and_cleans_up() {
    let moved = "fedcba9876543210fedcba9876543210fedcba98";
    let run = run_stubbed(
        &format!("{KIND_PATH}{APINATA}{DBENTO}"),
        &[("STUB_HEAD", moved)],
    );
    let err = String::from_utf8_lossy(&run.out.stderr);
    assert_eq!(run.out.status.code(), Some(1), "{err}");
    assert!(
        err.contains(&format!(
            "rules: apinata-rules tag apinata-rules-v0.1.0 is at {moved}, the lock pins 0123456789abcdef0123456789abcdef01234567"
        )),
        "{err}"
    );
    assert_eq!(run.cargo, "", "a crate was tested at a moved tag");
    assert!(run.left.is_empty(), "{:?}", run.left);
}

#[test]
fn a_failing_crate_test_fails_the_job_and_still_cleans_up() {
    let run = run_stubbed(
        &format!("{KIND_PATH}{APINATA}{DBENTO}"),
        &[("STUB_CARGO_FAIL", "1")],
    );
    assert_ne!(
        run.out.status.code(),
        Some(0),
        "a failing crate test passed the job"
    );
    assert!(
        run.cargo.starts_with("test -p apinata-rules --locked\n"),
        "{}",
        run.cargo
    );
    assert!(run.left.is_empty(), "{:?}", run.left);
}

#[test]
fn list_refuses_a_tag_git_could_misread() {
    for tag in ["apinata rules", "-apinata-rules"] {
        let lock = format!(
            "{KIND_PATH}{}",
            APINATA.replace("apinata-rules-v0.1.0", tag)
        );
        let out = run("list", &lock);
        assert_eq!(out.status.code(), Some(1), "{lock}");
        assert_eq!(stdout(&out), "", "{lock}");
    }
}

/// The tag Task 1 of the phase 1 plan confirmed on GitHub.
const APINATA_RULES_TAG: &str = "apinata-rules-v0.1.0";

#[test]
fn the_real_lock_pins_apinata_rules_at_its_tag() {
    let out = Command::new(repo_root().join("scripts/rules-crates.sh"))
        .args(["list", repo_root().join("Cargo.lock").to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "one rules crate pinned: {text}");
    let fields: Vec<&str> = text.split_whitespace().collect();
    assert_eq!(
        fields[..3],
        [
            "apinata-rules",
            "https://github.com/PhongGuy/apinata",
            APINATA_RULES_TAG
        ],
        "{text}"
    );
    assert!(
        fields[3].len() == 40 && fields[3].bytes().all(|b| b.is_ascii_hexdigit()),
        "{text}"
    );
}

#[test]
fn every_git_source_in_the_real_lock_is_public_https() {
    let lock = std::fs::read_to_string(repo_root().join("Cargo.lock")).unwrap();
    let sources: Vec<&str> = lock
        .lines()
        .filter_map(|l| l.strip_prefix("source = \"git+"))
        .collect();
    assert!(
        !sources.is_empty(),
        "no git source: the rules crate pin is missing"
    );
    for src in sources {
        assert!(src.starts_with("https://github.com/PhongGuy/"), "{src}");
        assert!(!src.contains('@'), "credentials in {src}");
    }
}
