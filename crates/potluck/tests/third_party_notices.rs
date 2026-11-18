//! scripts/third-party-crates.sh: the crate list in THIRD_PARTY_NOTICES.md, its licence guard,
//! and a staleness check against this checkout's dependency graph.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::common::{repo_root, write_script};

fn script() -> PathBuf {
    repo_root().join("scripts/third-party-crates.sh")
}

/// A crate as `cargo metadata` describes it, plus the top-level files of its source directory.
#[derive(Clone, Copy)]
struct Pkg {
    name: &'static str,
    version: &'static str,
    authors: &'static [&'static str],
    repository: Option<&'static str>,
    files: &'static [(&'static str, &'static str)],
}

/// A cargo stub: each call writes its arguments, one per line, to `args-<subcommand>` beside
/// it; `tree` prints `tree` and `metadata` prints a document whose packages live under `dir`.
fn stub_cargo(dir: &Path, tree: &str, pkgs: &[Pkg]) -> PathBuf {
    fs::write(dir.join("tree.txt"), tree).unwrap();
    let mut packages = Vec::new();
    for pkg in pkgs {
        let src = dir.join(format!("src/{}-{}", pkg.name, pkg.version));
        fs::create_dir_all(&src).unwrap();
        for (file, text) in pkg.files {
            fs::write(src.join(file), text).unwrap();
        }
        let authors: Vec<String> = pkg.authors.iter().map(|a| format!("{a:?}")).collect();
        packages.push(format!(
            r#"{{"name":"{}","version":"{}","manifest_path":"{}/Cargo.toml","authors":[{}],"repository":{}}}"#,
            pkg.name,
            pkg.version,
            src.display(),
            authors.join(","),
            pkg.repository.map_or("null".to_owned(), |r| format!("{r:?}")),
        ));
    }
    fs::write(
        dir.join("metadata.json"),
        format!(r#"{{"packages":[{}],"version":1}}"#, packages.join(",")),
    )
    .unwrap();
    let path = dir.join("cargo");
    write_script(
        &path,
        "#!/usr/bin/env bash\nhere=$(dirname \"$0\")\nprintf '%s\\n' \"$@\" > \"$here/args-$1\"\n\
         case $1 in tree) cat \"$here/tree.txt\" ;; metadata) cat \"$here/metadata.json\" ;; esac\n",
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

const MIT_BODY: &str = "Permission is hereby granted, free of charge, to any person obtaining a copy\n\
THE SOFTWARE IS PROVIDED \"AS IS\", IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT\n\
HOLDERS BE LIABLE FOR ANY CLAIM\n";

/// One crate per source of its notice: licence-file lines (several files, a duplicate, a
/// comment prefix, the Apache appendix template and a README that is not a licence file),
/// authors when the files carry no line, and the repository when neither exists.
const PKGS: &[Pkg] = &[
    Pkg {
        name: "potluck",
        version: "0.1.0",
        authors: &[],
        repository: None,
        files: &[],
    },
    Pkg {
        name: "zeta",
        version: "1.0.0",
        authors: &["Not Used <z@example.com>"],
        repository: None,
        files: &[("LICENSE", "MIT License\n\n\u{a9} 2019 Zeta\n")],
    },
    Pkg {
        name: "alpha",
        version: "0.2.0",
        authors: &["Not Used"],
        repository: Some("https://example.com/alpha"),
        files: &[
            ("LICENSE-MIT", "Copyright (c) 2020 Alpha Devs\n\n"),
            (
                "LICENSE-APACHE",
                "   Copyright [yyyy] [name of copyright owner]\n  Copyright 2021 Beta  \n",
            ),
            ("copyright", "Copyright (c) 2020 Alpha Devs\n"),
            ("NOTICE", "// Copyright 2015 Comment Person\n"),
            ("README.md", "Copyright 1999 Not A Licence File\n"),
        ],
    },
    Pkg {
        name: "alpha",
        version: "0.1.9",
        authors: &["A One <a@one.example>", "B Two"],
        repository: Some("https://example.com/alpha"),
        files: &[
            ("LICENSE-MIT", MIT_BODY),
            (
                "LICENSE-APACHE",
                "Copyright {yyyy} {name of copyright owner}\n",
            ),
        ],
    },
    Pkg {
        name: "icu",
        version: "2.0.0",
        authors: &[],
        repository: Some("https://example.com/icu"),
        files: &[("LICENSE", MIT_BODY)],
    },
];

const LIST: &str = "- `alpha` 0.1.9: Apache-2.0 WITH LLVM-exception OR MIT\n\
\x20 - Authors: A One <a@one.example>, B Two\n\
- `alpha` 0.2.0: MIT/Apache-2.0\n\
\x20 - Copyright (c) 2020 Alpha Devs\n\
\x20 - Copyright 2015 Comment Person\n\
\x20 - Copyright 2021 Beta\n\
- `icu` 2.0.0: Unicode-3.0\n\
\x20 - No copyright line or authors published; see https://example.com/icu\n\
- `zeta` 1.0.0: MIT\n\
\x20 - \u{a9} 2019 Zeta\n";

#[test]
fn prints_each_linked_crate_once_sorted_without_potluck() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&stub_cargo(dir.path(), TREE, PKGS), &[], None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), LIST);
}

#[test]
fn joins_a_copyright_statement_wrapped_across_lines() {
    // Only a header paragraph before the MIT grant that names a copyright counts; one that
    // does not leaves the crate to its authors.
    let wrapped = Pkg {
        name: "wrapped",
        version: "0.1.0",
        authors: &[],
        repository: Some("https://example.com/wrapped"),
        files: &[(
            "LICENSE-MIT",
            "This work is derived in part from the `older` crate, Copyright (c)\n\
             2015 The Older   Developers\n\nPermission is hereby granted, free of charge\n",
        )],
    };
    let plain = Pkg {
        name: "plain",
        version: "0.1.0",
        authors: &["P Lain"],
        repository: None,
        files: &[(
            "LICENSE-MIT",
            "The MIT License\n\nPermission is hereby granted, free of charge\n",
        )],
    };
    let tree = format!("{TREE}wrapped v0.1.0|MIT\nplain v0.1.0|MIT\n");
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &stub_cargo(dir.path(), &tree, &[PKGS, &[wrapped, plain]].concat()),
        &[],
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list = String::from_utf8_lossy(&out.stdout);
    assert!(
        list.contains(
            "- `wrapped` 0.1.0: MIT\n  - This work is derived in part from the `older` crate, \
             Copyright (c) 2015 The Older Developers\n- "
        ),
        "wrapped statement not joined: {list}"
    );
    assert!(
        list.contains("- `plain` 0.1.0: MIT\n  - Authors: P Lain\n"),
        "header without a copyright taken: {list}"
    );
}

#[test]
fn skips_only_path_crates_inside_this_repo() {
    // An in-repo crate is first-party; a path crate elsewhere (a [patch] to a local checkout)
    // is third-party and must still be listed and pass the licence guard.
    let root = fs::canonicalize(repo_root()).unwrap();
    let inside = format!(
        "potluck-kind v0.1.0 ({}/crates/potluck-kind)|MIT OR Apache-2.0\n",
        root.display()
    );
    let dir = tempfile::tempdir().unwrap();
    let tree = format!("{TREE}{inside}sibling v0.1.0 (/elsewhere/sibling)|MIT\n");
    let sibling = Pkg {
        name: "sibling",
        version: "0.1.0",
        authors: &[],
        repository: None,
        files: &[],
    };
    let out = run(
        &stub_cargo(dir.path(), &tree, &[PKGS, &[sibling]].concat()),
        &[],
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list = String::from_utf8_lossy(&out.stdout);
    assert!(
        !list.contains("`potluck-kind`"),
        "in-repo crate listed: {list}"
    );
    assert!(
        list.contains(
            "- `sibling` 0.1.0 (/elsewhere/sibling): MIT\n  \
             - No copyright line, authors or repository published\n"
        ),
        "out-of-tree path crate dropped: {list}"
    );

    let dir = tempfile::tempdir().unwrap();
    let tree = format!("{TREE}closed v0.1.0 (/elsewhere/closed)|LicenseRef-Proprietary\n");
    let out = run(&stub_cargo(dir.path(), &tree, PKGS), &[], None);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "out-of-tree path crate escaped the allowlist"
    );
    assert!(err.contains("closed"), "refusal does not name it: {err}");
}

#[test]
fn lists_a_git_sourced_crate() {
    // A rules crate pinned from another repository is linked into the binary, so it is
    // third-party here and must be listed and licence-checked like a registry crate.
    let dir = tempfile::tempdir().unwrap();
    let tree = format!(
        "{TREE}apinata-rules v0.1.0 (https://github.com/PhongGuy/apinata?tag=apinata-rules-v0.1.0#01234567)|MIT OR Apache-2.0\n"
    );
    let rules = Pkg {
        name: "apinata-rules",
        version: "0.1.0",
        authors: &[],
        repository: None,
        files: &[("LICENSE-MIT", "Copyright (c) 2026 Apinata Rules\n")],
    };
    let out = run(
        &stub_cargo(dir.path(), &tree, &[PKGS, &[rules]].concat()),
        &[],
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list = String::from_utf8_lossy(&out.stdout);
    assert!(
        list.contains(
            "- `apinata-rules` 0.1.0 (https://github.com/PhongGuy/apinata?tag=apinata-rules-v0.1.0#01234567): MIT OR Apache-2.0\n  \
             - Copyright (c) 2026 Apinata Rules\n"
        ),
        "git-sourced crate dropped: {list}"
    );
}

#[test]
fn asks_cargo_for_linked_dependencies_of_both_release_targets() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&stub_cargo(dir.path(), TREE, PKGS), &[], None);
    assert!(out.status.success());
    let args = fs::read_to_string(dir.path().join("args-tree")).unwrap();
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

    // Unfiltered metadata wants Windows and wasm sources offline, so it names both targets.
    let args = fs::read_to_string(dir.path().join("args-metadata")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(args.first(), Some(&"metadata"), "{args:?}");
    assert!(args.contains(&"--frozen"), "{args:?}");
    for pair in [
        ["--format-version", "1"],
        ["--filter-platform", "x86_64-unknown-linux-gnu"],
        ["--filter-platform", "aarch64-unknown-linux-gnu"],
    ] {
        assert!(
            args.windows(2).any(|w| w == pair),
            "cargo metadata args lack {pair:?}: {args:?}"
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
            &stub_cargo(dir.path(), &format!("{TREE}{line}\n"), PKGS),
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

    // A failed metadata call after a good tree is as fatal: no list without its notices.
    let dir = tempfile::tempdir().unwrap();
    let cargo = dir.path().join("cargo");
    fs::write(dir.path().join("tree.txt"), TREE).unwrap();
    write_script(
        &cargo,
        "#!/usr/bin/env bash\n[[ $1 == tree ]] && exec cat \"$(dirname \"$0\")/tree.txt\"\n\
         echo '{\"packages\":[]}'\nexit 101\n",
    );
    let out = run(&cargo, &[], None);
    assert!(!out.status.success(), "metadata failure ignored");
    assert!(out.stdout.is_empty());
}

#[test]
fn an_empty_crate_tree_fails_the_script() {
    for tree in ["", "\n\n"] {
        let dir = tempfile::tempdir().unwrap();
        let out = run(&stub_cargo(dir.path(), tree, PKGS), &[], None);
        assert!(!out.status.success(), "{tree:?}: accepted an empty tree");
        assert!(out.stdout.is_empty(), "{tree:?}: printed a list anyway");
    }
}

#[test]
fn a_crate_missing_from_cargo_metadata_fails_the_script() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&stub_cargo(dir.path(), TREE, &PKGS[..4]), &[], None);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "listed a crate without its notices");
    assert!(err.contains("icu"), "refusal does not name it: {err}");
    assert!(out.stdout.is_empty());
}

#[test]
fn write_replaces_only_the_marked_block() {
    let dir = tempfile::tempdir().unwrap();
    let cargo = stub_cargo(dir.path(), TREE, PKGS);
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

    // Exactly one begin marker must precede exactly one end marker, or nothing is written.
    let (b, e) = ("<!-- crates:begin -->\n", "<!-- crates:end -->\n");
    for text in [
        format!("# Head\n{b}"),
        format!("# Head\n{e}"),
        format!("# Head\n{e}x\n{b}"),
        format!("# Head\n{b}{b}x\n{e}"),
        format!("# Head\n{b}x\n{e}{e}"),
        format!("# Head\n{b}x\n{e}{b}y\n{e}"),
    ] {
        let bad = dir.path().join("BAD.md");
        fs::write(&bad, &text).unwrap();
        let out = run(&cargo, &["--write"], Some(&bad));
        assert!(!out.status.success(), "wrote into {text:?}");
        assert_eq!(fs::read_to_string(&bad).unwrap(), text);
    }
}

fn notices() -> String {
    fs::read_to_string(repo_root().join("THIRD_PARTY_NOTICES.md")).unwrap()
}

/// CI forces cargo's colour on; the script must print the same list as on a plain terminal.
#[test]
fn forced_cargo_colour_changes_nothing() {
    let run = |colour: &str| {
        Command::new(script())
            .env("CARGO", env!("CARGO"))
            .env("CARGO_TERM_COLOR", colour)
            .output()
            .unwrap()
    };
    let (plain, coloured) = (run("never"), run("always"));
    assert!(
        coloured.status.success(),
        "{}",
        String::from_utf8_lossy(&coloured.stderr)
    );
    assert_eq!(plain.stdout, coloured.stdout);
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
        "- `apinata-rules` 0.1.0 (https://github.com/PhongGuy/apinata?tag=apinata-rules-v0.1.0#",
        "- `apinata-format` 0.1.0 (https://github.com/PhongGuy/apinata?tag=apinata-rules-v0.1.0#",
        "- `saphyr-parser` ",
        "- `arraydeque` ",
        "- `yaml_serde` ",
        "- `dbento-rules` 0.1.0 (https://github.com/PhongGuy/dbento?tag=dbento-rules-v0.1.0#",
        "- `dbento-workspace` 0.2.0 (https://github.com/PhongGuy/dbento?tag=dbento-rules-v0.1.0#",
        "- `toml` ",
        "- `humantime` ",
        "- `winnow` ",
    ] {
        assert!(list.contains(present), "list lacks {present:?}");
    }
    for absent in [
        "- `potluck` ",
        "- `potluck-kind` ",
        "- `minisign` ",
        "- `serde_derive` ",
        "- `syn` ",
    ] {
        assert!(!list.contains(absent), "list carries {absent:?}");
    }
    // Each crate carries its own copyright line, read from the licence files it ships.
    for (name, notice) in [
        ("axum", "Copyright (c) 2019 axum Contributors"),
        (
            "aws-lc-sys",
            "Copyright (c) 1998-2011 The OpenSSL Project. All rights reserved.",
        ),
        ("anyhow", "Authors: David Tolnay <dtolnay@gmail.com>"),
        (
            "hashlink",
            "This work is derived in part from the `linked-hash-map` crate, \
             Copyright (c) 2015 The Rust Project Developers",
        ),
    ] {
        let block = list
            .split("\n- ")
            .find(|b| b.starts_with(&format!("`{name}` ")))
            .unwrap_or_else(|| panic!("{name} not listed"));
        assert!(
            block.contains(&format!("\n  - {notice}")),
            "{name} lacks {notice:?}"
        );
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
