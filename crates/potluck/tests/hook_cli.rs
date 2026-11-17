//! `potluck hook pre-receive` as git runs it: stdin lines, environment, exit code, stderr.

use std::io::Write;
use std::process::{Output, Stdio};

use crate::common::potluck;
use crate::common::scratch::Scratch;

const ZERO: &str = "0000000000000000000000000000000000000000";
const AWS: &[u8] = include_bytes!("fixtures/secret-check/bad/aws-access-key.txt");

/// Runs the hook against the scratch server repository with `env` on top of git's variables.
/// `HOME` and the two config variables keep git off the real user and system config.
fn run_hook(s: &Scratch, env: &[(&str, &str)], stdin: &str) -> Output {
    run_hook_bytes(s, env, stdin.as_bytes())
}

/// `run_hook` with stdin as raw bytes, for ref names that are not UTF-8.
fn run_hook_bytes(s: &Scratch, env: &[(&str, &str)], stdin: &[u8]) -> Output {
    let mut cmd = potluck();
    cmd.args(["hook", "pre-receive"])
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", s.dir.path())
        .env("GIT_DIR", &s.bare)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("POTLUCK_GIT", "git")
        .envs(env.iter().copied())
        .current_dir(s.dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn line(tip: &str) -> String {
    format!("{ZERO} {tip} refs/heads/main\n")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_refused_push_exits_1_with_the_contract_lines_only() {
    let s = Scratch::new();
    s.write(".env", b"x\n");
    s.write("creds.txt", AWS);
    let tip = s.stage_commit("bad");
    let out = run_hook(&s, &[], &line(&tip));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "potluck-reject secret_env_file refs/heads/main .env env_file\npotluck-reject secret_pattern refs/heads/main creds.txt aws\n"
    );
    assert!(out.stdout.is_empty(), "nothing on stdout");
}

#[test]
fn a_clean_push_and_an_empty_input_exit_0_silently() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    let tip = s.stage_commit("clean");
    for input in [line(&tip), String::new()] {
        let out = run_hook(&s, &[], &input);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert!(out.stderr.is_empty() && out.stdout.is_empty());
    }
}

#[test]
fn an_unknown_kind_refuses_with_one_error_line() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    let tip = s.stage_commit("r");
    let out = run_hook(&s, &[("POTLUCK_KIND", "dbento")], &line(&tip));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stderr(&out), "potluck-error unsupported kind\n");
}

#[test]
fn a_malformed_line_and_a_missing_git_fail_closed() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    let tip = s.stage_commit("clean");
    let out = run_hook(&s, &[], "garbage\n");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).starts_with("potluck-error "),
        "{}",
        stderr(&out)
    );
    assert_eq!(stderr(&out).lines().count(), 1);
    let out = run_hook(&s, &[("POTLUCK_GIT", "/nonexistent/git")], &line(&tip));
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).starts_with("potluck-error "),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_git_failure_reason_carries_no_git_output_paths_or_control_bytes() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    let tip = s.stage_commit("clean");
    let marker = "/srv/potluck-marker/creds.txt";
    let fake = s.script(
        "failing-git",
        &format!("printf 'fatal: {marker} \\033[31m\\a\\n' >&2\nexit 128\n"),
    );
    let out = run_hook(&s, &[("POTLUCK_GIT", fake.to_str().unwrap())], &line(&tip));
    assert_eq!(out.status.code(), Some(1));
    let errors: Vec<&[u8]> = out
        .stderr
        .split_inclusive(|&b| b == b'\n')
        .filter(|l| l.starts_with(b"potluck-error "))
        .collect();
    assert_eq!(errors.len(), 1, "{}", stderr(&out));
    let body = errors[0].strip_suffix(b"\n").expect("ends in a newline");
    assert!(
        !String::from_utf8_lossy(body).contains("potluck-marker"),
        "no git output: {}",
        stderr(&out)
    );
    assert!(
        body.iter().all(|&b| b >= 0x20),
        "no control bytes: {:?}",
        stderr(&out)
    );
}

#[test]
fn an_override_accepts_without_touching_git() {
    let s = Scratch::new();
    s.write(".env", b"x\n");
    let tip = s.stage_commit("bad");
    let out = run_hook(
        &s,
        &[
            ("POTLUCK_OVERRIDE", "5"),
            ("POTLUCK_GIT", "/nonexistent/git"),
        ],
        &line(&tip),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(out.stderr.is_empty());
}

#[test]
fn import_mode_scans_the_tip_tree_only() {
    let s = Scratch::new();
    s.write("creds.txt", AWS);
    s.commit("add");
    s.remove("creds.txt");
    let tip = s.stage_commit("remove");
    assert_eq!(
        stderr(&run_hook(&s, &[], &line(&tip))),
        "potluck-reject secret_pattern refs/heads/main creds.txt aws\n"
    );
    let out = run_hook(&s, &[("POTLUCK_IMPORT", "1")], &line(&tip));
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn a_ref_to_a_blob_is_scanned_and_reported_at_a_slash() {
    let s = Scratch::new();
    s.write("creds.txt", AWS);
    s.commit("source");
    s.tag("leak", &s.blob("main", "creds.txt"));
    let tag = s.stage("refs/tags/leak");
    let out = run_hook(&s, &[], &format!("{ZERO} {tag} refs/tags/leak\n"));
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "potluck-reject secret_pattern refs/tags/leak / aws\n"
    );
}

#[test]
fn a_token_in_a_commit_message_is_reported_at_a_slash() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    let tip = s.stage_commit(&format!("plk_{}", "0".repeat(43)));
    let out = run_hook(&s, &[], &line(&tip));
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "potluck-reject secret_pattern refs/heads/main / potluck\n"
    );
}

#[test]
fn a_ref_name_that_is_not_utf8_refuses_the_push_before_any_scan() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    let clean = s.stage_commit("clean");
    let caf = |old: &str, new: &str| {
        let mut l = format!("{old} {new} ").into_bytes();
        l.extend_from_slice(b"refs/heads/caf\xe9\n");
        l
    };
    // A creation, a deletion, and a creation beside a valid ref: each push is refused whole.
    let mut beside = line(&clean).into_bytes();
    beside.extend(caf(ZERO, &clean));
    for input in [caf(ZERO, &clean), caf(&clean, ZERO), beside] {
        let out = run_hook_bytes(&s, &[], &input);
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert_eq!(stderr(&out), "ref name is not valid UTF-8\n");
        assert!(out.stdout.is_empty(), "nothing on stdout");
    }
    // No git runs: a missing git gives the same line, not a potluck-error.
    let out = run_hook_bytes(
        &s,
        &[("POTLUCK_GIT", "/nonexistent/git")],
        &caf(ZERO, &clean),
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(stderr(&out), "ref name is not valid UTF-8\n");
    // Secrets on such a ref add no potluck-reject lines: nothing is scanned.
    s.write("creds.txt", AWS);
    let bad = s.stage_commit("bad");
    let out = run_hook_bytes(&s, &[], &caf(ZERO, &bad));
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(stderr(&out), "ref name is not valid UTF-8\n");
}

#[test]
fn each_hook_failure_writes_its_own_fixed_reason() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    let tip = s.stage_commit("clean");
    // Exits 0 with output `cat-file --batch-check` never gives; it drains stdin first so the
    // write of the object name cannot fail.
    let garbled = s.script("garbled-git", "cat >/dev/null\necho garbage\n");
    let cases = [
        ("garbage\n".to_owned(), "git", "malformed ref line"),
        (line(&tip), "/nonexistent/git", "cannot run git cat-file"),
        (
            line(&tip),
            garbled.to_str().unwrap(),
            "unexpected git cat-file output",
        ),
    ];
    for (input, git, reason) in cases {
        let out = run_hook(&s, &[("POTLUCK_GIT", git)], &input);
        assert_eq!(out.status.code(), Some(1), "{reason}: {}", stderr(&out));
        assert_eq!(stderr(&out), format!("potluck-error {reason}\n"));
    }
    // A directory as stdin opens but cannot be read.
    let out = potluck()
        .args(["hook", "pre-receive"])
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", s.dir.path())
        .env("GIT_DIR", &s.bare)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("POTLUCK_GIT", "git")
        .current_dir(s.dir.path())
        .stdin(std::fs::File::open(s.dir.path()).unwrap())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(stderr(&out), "potluck-error unreadable stdin\n");
}

#[test]
fn an_unlisted_format_prints_the_format_code() {
    let s = Scratch::new();
    s.write("opencollection.yml", b"opencollection: 9.9.9\n");
    let tip = s.stage_commit("c");
    let out = run_hook(&s, &[("POTLUCK_KIND", "apinata")], &line(&tip));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "potluck-reject format_unsupported refs/heads/main opencollection.yml format_unsupported\n"
    );
}
