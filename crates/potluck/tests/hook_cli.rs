//! `potluck hook pre-receive` as git runs it: stdin lines, environment, exit code, stderr.

use std::io::Write;
use std::process::{Output, Stdio};

use crate::common::potluck;
use crate::common::scratch::Scratch;

const ZERO: &str = "0000000000000000000000000000000000000000";
const AWS: &[u8] = include_bytes!("fixtures/secret-check/bad/aws-access-key.txt");
const ENV_YML: &[u8] = include_bytes!("fixtures/secret-check/bad/env-secret-valued.yml");

/// Runs the hook against the scratch server repository with `env` on top of git's variables.
/// `HOME` and the two config variables keep git off the real user and system config.
fn run_hook(s: &Scratch, env: &[(&str, &str)], stdin: &str) -> Output {
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
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
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
fn an_unknown_kind_warns_once_and_scans_generic_only() {
    let s = Scratch::new();
    s.write("environments/dev.yml", ENV_YML);
    let tip = s.stage_commit("env");
    let out = run_hook(&s, &[("POTLUCK_KIND", "dbento")], &line(&tip));
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "potluck-warning unknown kind \"dbento\"; generic rules only\n"
    );
    let out = run_hook(&s, &[("POTLUCK_KIND", "apinata")], &line(&tip));
    assert_eq!(
        stderr(&out),
        "potluck-reject secret_value refs/heads/main environments/dev.yml secret_value\n"
    );
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
