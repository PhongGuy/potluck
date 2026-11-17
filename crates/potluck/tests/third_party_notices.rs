//! scripts/third-party-crates.sh: the crate list in THIRD_PARTY_NOTICES.md, its licence guard,
//! and a staleness check against this checkout's dependency graph.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::common::{repo_root, write_script};

fn script() -> PathBuf {
    repo_root().join("scripts/third-party-crates.sh")
}

/// A cargo stub that writes its arguments, one per line, to `args` beside it and prints `tree`.
fn stub_cargo(dir: &Path, tree: &str) -> PathBuf {
    fs::write(dir.join("tree.txt"), tree).unwrap();
    let path = dir.join("cargo");
    write_script(
        &path,
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/args\"\n\
         cat \"$(dirname \"$0\")/tree.txt\"\n",
    );
    path
}

fn run(cargo: &Path, args: &[&str], notices: Option<&Path>) -> Output {
    let mut cmd = Command::new(script());
    cmd.args(args).env("CARGO", cargo);
    if let Some(notices) = notices {
        cmd.env("NOTICES", notices);
    }
    cmd.output().unwrap()
}

/// Two per-target trees as cargo prints them: a root line with a path, dedupe markers, a blank
/// line between the trees, and a `/` licence separator.
const TREE: &str = "potluck v0.1.0 (/src/crates/potluck)|MIT OR Apache-2.0\n\
zeta v1.0.0|MIT\n\
alpha v0.2.0|MIT/Apache-2.0\n\
alpha v0.2.0|MIT/Apache-2.0 (*)\n\
\n\
potluck v0.1.0 (/src/crates/potluck)|MIT OR Apache-2.0\n\
alpha v0.1.9|Apache-2.0 WITH LLVM-exception OR MIT\n\
icu v2.0.0|Unicode-3.0\n";

const LIST: &str = "- `alpha` 0.1.9: Apache-2.0 WITH LLVM-exception OR MIT\n\
- `alpha` 0.2.0: MIT/Apache-2.0\n\
- `icu` 2.0.0: Unicode-3.0\n\
- `zeta` 1.0.0: MIT\n";

#[test]
fn prints_each_linked_crate_once_sorted_without_potluck() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&stub_cargo(dir.path(), TREE), &[], None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), LIST);
}

#[test]
fn asks_cargo_for_linked_dependencies_of_both_release_targets() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&stub_cargo(dir.path(), TREE), &[], None);
    assert!(out.status.success());
    let args = fs::read_to_string(dir.path().join("args")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(args.first(), Some(&"tree"), "{args:?}");
    assert!(args.contains(&"--frozen"), "{args:?}");
    for pair in [
        ["-p", "potluck"],
        ["-e", "normal,no-proc-macro"],
        ["--target", "x86_64-unknown-linux-gnu"],
        ["--target", "aarch64-unknown-linux-gnu"],
        ["--prefix", "none"],
        ["-f", "{p}|{l}"],
    ] {
        assert!(
            args.windows(2).any(|w| w == pair),
            "cargo args lack {pair:?}: {args:?}"
        );
    }
}

#[test]
fn refuses_a_crate_with_an_unknown_or_missing_licence() {
    for (line, name) in [
        ("closed v1.0.0|LicenseRef-Proprietary", "closed"),
        ("bare v1.0.0|", "bare"),
        ("eula v1.0.0|MIT AND Commercial", "eula"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let out = run(
            &stub_cargo(dir.path(), &format!("{TREE}{line}\n")),
            &[],
            None,
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{line}: accepted");
        assert!(
            err.contains(name),
            "{line}: refusal does not name it: {err}"
        );
        assert!(out.stdout.is_empty(), "{line}: printed a list anyway");
    }
}

#[test]
fn a_failed_cargo_run_fails_the_script() {
    let dir = tempfile::tempdir().unwrap();
    let cargo = dir.path().join("cargo");
    write_script(
        &cargo,
        "#!/usr/bin/env bash\necho 'alpha v1.0.0|MIT'\nexit 101\n",
    );
    let out = run(&cargo, &[], None);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}

#[test]
fn write_replaces_only_the_marked_block() {
    let dir = tempfile::tempdir().unwrap();
    let cargo = stub_cargo(dir.path(), TREE);
    let notices = dir.path().join("NOTICES.md");
    fs::write(
        &notices,
        "# Head\n\n<!-- crates:begin -->\n- `old` 1.0.0: MIT\n<!-- crates:end -->\n\n## Tail\n",
    )
    .unwrap();
    let out = run(&cargo, &["--write"], Some(&notices));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(&notices).unwrap(),
        format!("# Head\n\n<!-- crates:begin -->\n{LIST}<!-- crates:end -->\n\n## Tail\n")
    );

    let unmarked = dir.path().join("UNMARKED.md");
    fs::write(&unmarked, "# Head\n").unwrap();
    let out = run(&cargo, &["--write"], Some(&unmarked));
    assert!(!out.status.success(), "wrote into a file without markers");
    assert_eq!(fs::read_to_string(&unmarked).unwrap(), "# Head\n");
}

fn notices() -> String {
    fs::read_to_string(repo_root().join("THIRD_PARTY_NOTICES.md")).unwrap()
}

/// The committed block is exactly what the script prints from this checkout's Cargo.lock.
#[test]
fn the_committed_crate_list_is_current() {
    let out = Command::new(script())
        .env("CARGO", env!("CARGO"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list = String::from_utf8_lossy(&out.stdout);
    let text = notices();
    let begin = "<!-- crates:begin -->\n";
    let start = text.find(begin).expect("no begin marker") + begin.len();
    let stop = start
        + text[start..]
            .find("<!-- crates:end -->")
            .expect("no end marker");
    assert_eq!(
        &text[start..stop],
        list,
        "THIRD_PARTY_NOTICES.md is stale: run scripts/third-party-crates.sh --write"
    );
    // Linked crates are listed; dev-dependencies, proc-macros and potluck itself are not.
    for present in [
        "- `axum` ",
        "- `aws-lc-sys` ",
        "- `rustls` ",
        "- `minisign-verify` ",
    ] {
        assert!(list.contains(present), "list lacks {present:?}");
    }
    for absent in [
        "- `potluck` ",
        "- `minisign` ",
        "- `serde_derive` ",
        "- `syn` ",
    ] {
        assert!(!list.contains(absent), "list carries {absent:?}");
    }
}

#[test]
fn the_notices_carry_every_licence_text_a_listed_crate_requires() {
    let text = notices();
    for needle in [
        "## Rust crates",
        "scripts/third-party-crates.sh",
        "LICENSE-APACHE",
        "ISC License:",
        "MIT License:",
        "BSD 3-Clause License:",
        "UNICODE LICENSE V3",
        "This notice may not be removed or altered from any source distribution.",
    ] {
        assert!(text.contains(needle), "notices lack {needle:?}");
    }
}
