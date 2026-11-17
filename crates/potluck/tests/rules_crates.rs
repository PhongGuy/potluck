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

/// Runs `test` mode over a lock with stub `git` and `cargo` first on PATH, so no network is
/// reached. Returns the output, each cargo call's arguments, and what is left in its TMPDIR.
fn run_stubbed(lock: &str, cargo_reads_stdin: bool) -> (Output, String, Vec<std::path::PathBuf>) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (bin, tmp) = (dir.path().join("bin"), dir.path().join("tmp"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    // The stub clone creates its target dir and every clone's HEAD is the pinned commit.
    let git = "#!/bin/sh\nif [ \"$1\" = clone ]; then for a; do last=$a; done; mkdir -p \"$last\"; exit 0; fi\nif [ \"$1\" = -C ]; then echo 0123456789abcdef0123456789abcdef01234567; exit 0; fi\nexit 2\n";
    let cargo = "#!/bin/sh\nif [ -n \"$STUB_READ_STDIN\" ]; then cat > /dev/null; fi\necho \"$*\" >> \"$STUB_LOG\"\n";
    for (name, body) in [("git", git), ("cargo", cargo)] {
        let path = bin.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let lock_path = dir.path().join("Cargo.lock");
    std::fs::write(&lock_path, format!("version = 4\n\n{lock}")).unwrap();
    let log = dir.path().join("cargo.log");
    let mut cmd = Command::new(repo_root().join("scripts/rules-crates.sh"));
    cmd.args(["test", lock_path.to_str().unwrap()])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("TMPDIR", &tmp)
        .env("STUB_LOG", &log);
    if cargo_reads_stdin {
        cmd.env("STUB_READ_STDIN", "1");
    }
    let out = cmd.output().unwrap();
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    let left = std::fs::read_dir(&tmp)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    (out, calls, left)
}

const BOTH_TESTED: &str = "test -p apinata-rules --locked\ntest -p dbento-rules --locked\n";

#[test]
fn test_runs_every_pinned_crate_and_removes_its_clones() {
    let (out, calls, left) = run_stubbed(&format!("{KIND_PATH}{APINATA}{DBENTO}"), false);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(calls, BOTH_TESTED);
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_crate_test_reading_stdin_does_not_skip_the_next_crate() {
    let (out, calls, _) = run_stubbed(&format!("{KIND_PATH}{APINATA}{DBENTO}"), true);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(calls, BOTH_TESTED);
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
