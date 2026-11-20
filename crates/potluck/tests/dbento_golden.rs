//! The exact bytes the hook prints for a dbento workspace, judged by the pinned `dbento-rules`:
//! Dbento's own format fixtures, its path rules, and its safety relaxations by role. Detail lines
//! are contract bytes too; Dbento parses these lines.

use std::io::Write;
use std::process::{Output, Stdio};

use potluck::secret_check::git::{Git, RefUpdate};

use crate::common::potluck;
use crate::common::scratch::Scratch;

const ZERO: &str = "0000000000000000000000000000000000000000";
const READ_ONLY: &[u8] = include_bytes!("fixtures/dbento/valid/read-only.toml");
const FOLDERS: &[u8] = include_bytes!("fixtures/dbento/valid/folder-colours.toml");
const SEED: &[u8] = include_bytes!("fixtures/dbento/valid/seed.toml");
const RSA_KEY: &[u8] = include_bytes!("fixtures/secret-check/bad/rsa-private-key.pem");
const CERT: &[u8] = include_bytes!("fixtures/secret-check/good/certificate.pem");

/// Dbento's valid fixtures at the pinned tag: each opens in Dbento, so a push of it is accepted.
const VALID: [(&str, &[u8]); 4] = [
    (
        "every-engine",
        include_bytes!("fixtures/dbento/valid/every-engine.toml"),
    ),
    ("folder-colours", FOLDERS),
    ("read-only", READ_ONLY),
    ("seed", SEED),
];

/// Dbento's invalid fixtures at the pinned tag, each with the stderr of a push that puts it at
/// the root over the seed. A file whose format cannot be read also fails the format pass.
const INVALID: [(&str, &[u8], &str); 17] = [
    (
        "bad-colour",
        include_bytes!("fixtures/dbento/invalid/bad-colour.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml invalid\n",
            "potluck-reject-detail connections.a\n",
        ),
    ),
    (
        "bad-name",
        include_bytes!("fixtures/dbento/invalid/bad-name.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml invalid_name\n",
            "potluck-reject-detail connections.\"bad%20name\"\n",
        ),
    ),
    (
        "bad-toml",
        include_bytes!("fixtures/dbento/invalid/bad-toml.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml parse\n",
            "potluck-reject format_unsupported refs/heads/main dbento.toml format_unsupported\n",
        ),
    ),
    (
        "dynamodb-endpoint-scheme-first",
        include_bytes!("fixtures/dbento/invalid/dynamodb-endpoint-scheme-first.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml invalid\n",
            "potluck-reject-detail connections.items\n",
        ),
    ),
    (
        "dynamodb-endpoint-userinfo",
        include_bytes!("fixtures/dbento/invalid/dynamodb-endpoint-userinfo.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml secret\n",
            "potluck-reject-detail connections.items.endpoint\n",
        ),
    ),
    (
        "mongodb-uri-at-after-port",
        include_bytes!("fixtures/dbento/invalid/mongodb-uri-at-after-port.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml secret\n",
            "potluck-reject-detail connections.docs.uri\n",
        ),
    ),
    (
        "mongodb-uri-bad-port",
        include_bytes!("fixtures/dbento/invalid/mongodb-uri-bad-port.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml invalid\n",
            "potluck-reject-detail connections.docs\n",
        ),
    ),
    (
        "mongodb-uri-password",
        include_bytes!("fixtures/dbento/invalid/mongodb-uri-password.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml secret\n",
            "potluck-reject-detail connections.docs.uri\n",
        ),
    ),
    (
        "mongodb-uri-secret-option",
        include_bytes!("fixtures/dbento/invalid/mongodb-uri-secret-option.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml secret\n",
            "potluck-reject-detail connections.docs.uri\n",
        ),
    ),
    (
        "password-connection",
        include_bytes!("fixtures/dbento/invalid/password-connection.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml password\n",
            "potluck-reject-detail connections.a.password\n",
        ),
    ),
    (
        "password-nested",
        include_bytes!("fixtures/dbento/invalid/password-nested.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml password\n",
            "potluck-reject-detail connections.a.settings.password\n",
        ),
    ),
    (
        "password-top",
        include_bytes!("fixtures/dbento/invalid/password-top.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml password\n",
            "potluck-reject-detail password\n",
        ),
    ),
    (
        "password-wins",
        include_bytes!("fixtures/dbento/invalid/password-wins.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml password\n",
            "potluck-reject-detail connections.a.password\n",
        ),
    ),
    (
        "unknown-connection-key",
        include_bytes!("fixtures/dbento/invalid/unknown-connection-key.toml"),
        "potluck-reject secret_value refs/heads/main dbento.toml parse\n",
    ),
    (
        "unknown-root-key",
        include_bytes!("fixtures/dbento/invalid/unknown-root-key.toml"),
        "potluck-reject secret_value refs/heads/main dbento.toml parse\n",
    ),
    (
        "version-missing",
        include_bytes!("fixtures/dbento/invalid/version-missing.toml"),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml parse\n",
            "potluck-reject format_unsupported refs/heads/main dbento.toml format_unsupported\n",
        ),
    ),
    (
        "version-wrong",
        include_bytes!("fixtures/dbento/invalid/version-wrong.toml"),
        concat!(
            "potluck-reject format_unsupported refs/heads/main dbento.toml unsupported_version\n",
            "potluck-reject format_unsupported refs/heads/main dbento.toml format_unsupported\n",
        ),
    ),
];

/// The hook as git runs it, kind dbento, with `POTLUCK_ROLE` set only when `role` is given.
fn run_hook(s: &Scratch, role: Option<&str>, stdin: &str) -> Output {
    let mut cmd = potluck();
    cmd.args(["hook", "pre-receive"])
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", s.dir.path())
        .env("GIT_DIR", &s.bare)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("POTLUCK_GIT", "git")
        .env("POTLUCK_KIND", "dbento")
        .current_dir(s.dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(role) = role {
        cmd.env("POTLUCK_ROLE", role);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// A server whose `main` holds `base` as `dbento.toml`, and a pushed commit on top that writes
/// `files`. Returns the scratch, the old and new tips, and the hook's output as `role`.
fn push_onto(
    base: &[u8],
    files: &[(&str, &[u8])],
    role: Option<&str>,
) -> (Scratch, String, String, Output) {
    let s = Scratch::new();
    s.write("dbento.toml", base);
    let old = s.commit("base");
    s.publish("main");
    for (path, bytes) in files {
        s.write(path, bytes);
    }
    let new = s.stage_commit("change");
    let out = run_hook(&s, role, &format!("{old} {new} refs/heads/main\n"));
    (s, old, new, out)
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// The read-only fixture with its first `from` replaced by `to`; panics if the fixture lacks it.
fn read_only_with(from: &str, to: &str) -> Vec<u8> {
    let text = std::str::from_utf8(READ_ONLY).unwrap();
    assert!(text.contains(from), "the read-only fixture lacks {from:?}");
    text.replacen(from, to, 1).into_bytes()
}

#[test]
fn every_valid_fixture_is_accepted_on_a_new_branch() {
    for (case, bytes) in VALID {
        let s = Scratch::new();
        s.write("dbento.toml", bytes);
        let tip = s.stage_commit(case);
        let out = run_hook(&s, None, &format!("{ZERO} {tip} refs/heads/main\n"));
        assert_eq!(out.status.code(), Some(0), "{case}: {}", stderr(&out));
        assert_eq!(stderr(&out), "", "{case}");
    }
}

#[test]
fn every_invalid_fixture_is_refused_with_these_bytes() {
    for (case, bytes, expected) in INVALID {
        let (_s, _, _, out) = push_onto(SEED, &[("dbento.toml", bytes)], None);
        assert_eq!(out.status.code(), Some(1), "{case}");
        assert_eq!(stderr(&out), expected, "{case}");
    }
}

#[test]
fn database_and_key_files_are_refused_by_name_and_a_certificate_passes() {
    let (_s, _, _, out) = push_onto(
        SEED,
        &[
            ("data/app.db", b"x"),
            ("a/b.SQLITE3-wal", b"x"),
            ("x.duckdb.wal", b"x"),
            ("certs/client.key", b"x"),
            ("k.p12", b"x"),
            ("certs/ca.pem", CERT),
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        concat!(
            "potluck-reject secret_env_file refs/heads/main a/b.SQLITE3-wal database_file\n",
            "potluck-reject secret_env_file refs/heads/main certs/client.key key_file\n",
            "potluck-reject secret_env_file refs/heads/main data/app.db database_file\n",
            "potluck-reject secret_env_file refs/heads/main k.p12 key_file\n",
            "potluck-reject secret_env_file refs/heads/main x.duckdb.wal database_file\n",
        )
    );
}

#[test]
fn a_pem_private_key_is_refused_by_the_generic_pattern() {
    // dbento-rules leaves `.pem` to content: a certificate passes, a key is the generic rule's.
    let (_s, _, _, out) = push_onto(SEED, &[("certs/client.pem", RSA_KEY)], None);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "potluck-reject secret_pattern refs/heads/main certs/client.pem private_key\n"
    );
}

#[test]
fn a_dbento_toml_below_the_root_in_any_case_is_read_too() {
    let (_s, _, _, out) = push_onto(
        SEED,
        &[("sub/DBENTO.toml", b"dbento = 1\npassword = \"x\"\n")],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        concat!(
            "potluck-reject secret_value refs/heads/main sub/DBENTO.toml password\n",
            "potluck-reject-detail password\n",
        )
    );
}

#[test]
fn an_unreadable_root_dbento_toml_fails_closed() {
    let big = vec![b'#'; 1024 * 1024 + 1];
    let (_s, _, _, out) = push_onto(SEED, &[("dbento.toml", &big)], None);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml too_large\n",
            "potluck-reject format_unsupported refs/heads/main dbento.toml format_unsupported\n",
        )
    );
    let (_s, _, _, out) = push_onto(SEED, &[("dbento.toml", b"dbento = 1\n# \xff\n")], None);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml parse\n",
            "potluck-reject format_unsupported refs/heads/main dbento.toml format_unsupported\n",
        )
    );
}

#[test]
fn a_relaxed_read_only_flag_is_refused_for_every_role_but_admin() {
    let relaxed = read_only_with("read_only = true\n", "read_only = false\n");
    // An unset or unknown role reads as write.
    for role in [None, Some("read"), Some("write"), Some("owner")] {
        let (_s, _, _, out) = push_onto(READ_ONLY, &[("dbento.toml", &relaxed)], role);
        assert_eq!(out.status.code(), Some(1), "{role:?}");
        assert_eq!(
            stderr(&out),
            concat!(
                "potluck-reject safety_relaxation refs/heads/main dbento.toml relax_read_only\n",
                "potluck-reject-detail connections.prod.read_only\n",
            ),
            "{role:?}"
        );
    }
    let (_s, _, _, out) = push_onto(READ_ONLY, &[("dbento.toml", &relaxed)], Some("admin"));
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out), "");
}

#[test]
fn a_relaxed_red_colour_is_refused_whether_own_or_from_a_folder() {
    let own = read_only_with("color = \"red\"\n", "color = \"orange\"\n");
    let (_s, _, _, out) = push_onto(READ_ONLY, &[("dbento.toml", &own)], Some("write"));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        concat!(
            "potluck-reject safety_relaxation refs/heads/main dbento.toml relax_color\n",
            "potluck-reject-detail connections.prod.color\n",
        )
    );
    let text = std::str::from_utf8(FOLDERS).unwrap();
    let from = "[folders.\"Prod\"]\ncolor = \"red\"\n";
    assert!(
        text.contains(from),
        "the folder-colours fixture lacks {from:?}"
    );
    let folder = text.replacen(from, "[folders.\"Prod\"]\ncolor = \"blue\"\n", 1);
    let (_s, _, _, out) = push_onto(
        FOLDERS,
        &[("dbento.toml", folder.as_bytes())],
        Some("write"),
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        concat!(
            "potluck-reject safety_relaxation refs/heads/main dbento.toml relax_color\n",
            "potluck-reject-detail connections.us-main.color\n",
        )
    );
}

#[test]
fn two_relaxations_in_one_file_print_both_each_with_its_detail() {
    let both = read_only_with(
        "read_only = true\ncolor = \"red\"\n",
        "read_only = false\ncolor = \"orange\"\n",
    );
    let (_s, _, _, out) = push_onto(READ_ONLY, &[("dbento.toml", &both)], Some("write"));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        concat!(
            "potluck-reject safety_relaxation refs/heads/main dbento.toml relax_read_only\n",
            "potluck-reject-detail connections.prod.read_only\n",
            "potluck-reject safety_relaxation refs/heads/main dbento.toml relax_color\n",
            "potluck-reject-detail connections.prod.color\n",
        )
    );
}

#[test]
fn deleting_a_guarded_connection_is_not_a_relaxation() {
    let (_s, _, _, out) = push_onto(
        READ_ONLY,
        &[("dbento.toml", b"dbento = 1\n")],
        Some("write"),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out), "");
}

#[test]
fn an_admin_relaxation_is_accepted_then_logged_by_the_recheck() {
    let relaxed = read_only_with("read_only = true\n", "read_only = false\n");
    let (s, old, new, out) = push_onto(READ_ONLY, &[("dbento.toml", &relaxed)], Some("admin"));
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let git = Git::new(s.isolated_git(), Some(s.bare.clone()));
    let update = RefUpdate::parse_line(format!("{old} {new} refs/heads/main").as_bytes()).unwrap();
    let text = crate::logs::captured_logs(|| {
        potluck::secret_check::log_accepted_relaxations(
            7,
            "alice",
            potluck::kinds::by_name("dbento").expect("dbento is registered"),
            &[update],
            &git,
        );
    });
    let line = text
        .lines()
        .find(|l| l.contains("safety relaxation accepted"))
        .unwrap_or_else(|| panic!("{text}"));
    for needle in [
        " INFO ",
        "workspace_id=7",
        "alice",
        "refs/heads/main",
        "relax_read_only",
    ] {
        assert!(line.contains(needle), "{needle} in {line}");
    }
}

#[test]
fn an_admin_push_relaxing_two_rules_is_accepted_and_logs_both() {
    let both = read_only_with(
        "read_only = true\ncolor = \"red\"\n",
        "read_only = false\ncolor = \"orange\"\n",
    );
    let (s, old, new, out) = push_onto(READ_ONLY, &[("dbento.toml", &both)], Some("admin"));
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out), "");
    let git = Git::new(s.isolated_git(), Some(s.bare.clone()));
    let update = RefUpdate::parse_line(format!("{old} {new} refs/heads/main").as_bytes()).unwrap();
    let text = crate::logs::captured_logs(|| {
        potluck::secret_check::log_accepted_relaxations(
            7,
            "alice",
            potluck::kinds::by_name("dbento").expect("dbento is registered"),
            &[update],
            &git,
        );
    });
    let logged: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("safety relaxation accepted"))
        .map(|l| {
            ["relax_read_only", "relax_color"]
                .into_iter()
                .find(|rule| l.contains(rule))
                .unwrap_or(l)
        })
        .collect();
    assert_eq!(logged, ["relax_read_only", "relax_color"], "{text}");
}
