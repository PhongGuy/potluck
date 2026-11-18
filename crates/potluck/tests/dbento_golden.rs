//! The exact bytes the hook prints for a dbento workspace, judged by the pinned `dbento-rules`:
//! Dbento's own format fixtures, its path rules, and its safety relaxations by role. Detail lines
//! are contract bytes too; Dbento parses these lines.

use std::io::Write;
use std::process::{Output, Stdio};

use crate::common::potluck;
use crate::common::scratch::Scratch;

const ZERO: &str = "0000000000000000000000000000000000000000";
const READ_ONLY: &[u8] = include_bytes!("fixtures/dbento/valid/read-only.toml");
const FOLDERS: &[u8] = include_bytes!("fixtures/dbento/valid/folder-colours.toml");
const SEED: &[u8] = include_bytes!("fixtures/dbento/valid/seed.toml");

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
