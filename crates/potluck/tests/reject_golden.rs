//! The exact bytes the hook prints for today's findings: the Apinata client parses these lines,
//! so any change to them is a contract change, not a refactor.

use std::io::Write;
use std::process::{Output, Stdio};

use crate::common::potluck;
use crate::common::scratch::Scratch;

const ZERO: &str = "0000000000000000000000000000000000000000";
const AWS: &[u8] = include_bytes!("fixtures/secret-check/bad/aws-access-key.txt");
const ENV_YML: &[u8] = include_bytes!("fixtures/secret-check/bad/env-secret-valued.yml");
const NOT_YAML: &[u8] = include_bytes!("fixtures/secret-check/bad/env-not-yaml.yml");
const PLK: &[u8] = include_bytes!("fixtures/secret-check/bad/potluck-token.txt");
const SEED_COLLECTION: &[u8] = b"opencollection: 1.0.0\n\ninfo:\n  name: Workspace\nbundled: false\nextensions:\n  bruno:\n    ignore:\n      - node_modules\n      - .git\n";

/// The hook as git runs it, kind apinata, against the scratch server repository.
fn run_hook(s: &Scratch, stdin: &str) -> Output {
    let mut child = potluck()
        .args(["hook", "pre-receive"])
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", s.dir.path())
        .env("GIT_DIR", &s.bare)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("POTLUCK_GIT", "git")
        .env("POTLUCK_KIND", "apinata")
        .current_dir(s.dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// A seeded workspace on the server; `main` then gains every kind of finding in one update, and
/// a new `feat` branch adds an oversized environment file and a path that needs encoding.
#[test]
fn todays_findings_print_exactly_these_bytes() {
    let s = Scratch::new();
    s.write("opencollection.yml", SEED_COLLECTION);
    let seed = s.commit("seed");
    s.publish("main");
    s.write(".env", b"KEY=fixture\n");
    s.write("creds.txt", AWS);
    s.write("environments/dev.yml", ENV_YML);
    s.write("environments/bad.yml", NOT_YAML);
    s.write("notes/with space.txt", AWS);
    s.git(Some(&s.work), &["add", "-A"]);
    let message = format!("add\n\n{}", String::from_utf8_lossy(PLK).trim());
    s.git(Some(&s.work), &["commit", "-q", "-m", &message]);
    let main = s.stage("main");
    s.branch("feat");
    s.write("environments/big.yml", &vec![b'a'; 1024 * 1024 + 1]);
    s.write("odd%name\t.txt", AWS);
    s.commit("feat");
    let feat = s.stage("feat");
    let stdin = format!("{seed} {main} refs/heads/main\n{ZERO} {feat} refs/heads/feat\n");
    let out = run_hook(&s, &stdin);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        concat!(
            "potluck-reject secret_env_file refs/heads/main .env env_file\n",
            "potluck-reject secret_pattern refs/heads/main creds.txt aws\n",
            "potluck-reject secret_value refs/heads/main environments/bad.yml env_unparsable\n",
            "potluck-reject secret_value refs/heads/main environments/dev.yml secret_value\n",
            "potluck-reject secret_pattern refs/heads/main notes/with%20space.txt aws\n",
            "potluck-reject secret_pattern refs/heads/main / potluck\n",
            "potluck-reject secret_value refs/heads/feat environments/big.yml env_unparsable\n",
            "potluck-reject secret_pattern refs/heads/feat odd%25name%09.txt aws\n",
        )
    );
}

/// Past twenty findings the rest is one count line.
#[test]
fn past_twenty_findings_the_rest_is_counted() {
    let s = Scratch::new();
    s.write("opencollection.yml", SEED_COLLECTION);
    for i in 0..22 {
        s.write(&format!("d{i:02}/.env"), b"KEY=fixture\n");
    }
    let tip = s.stage_commit("many");
    let out = run_hook(&s, &format!("{ZERO} {tip} refs/heads/main\n"));
    assert_eq!(out.status.code(), Some(1));
    let mut expected: String = (0..20)
        .map(|i| format!("potluck-reject secret_env_file refs/heads/main d{i:02}/.env env_file\n"))
        .collect();
    expected.push_str("potluck-reject-more 2\n");
    assert_eq!(String::from_utf8(out.stderr).unwrap(), expected);
}
