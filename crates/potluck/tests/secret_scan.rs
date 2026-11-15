//! The whole check over staged pushes.

use std::path::PathBuf;

use potluck::secret_check::git::{Git, RefUpdate};
use potluck::secret_check::scan::{HookEnv, Rejection, ScanError, scan};

use crate::common::scratch::Scratch;

const ZERO: &str = "0000000000000000000000000000000000000000";
const AWS: &[u8] = include_bytes!("fixtures/secret-check/bad/aws-access-key.txt");
const ENV_YML: &[u8] = include_bytes!("fixtures/secret-check/bad/env-secret-valued.yml");

fn git_for(s: &Scratch) -> Git {
    Git::new(s.isolated_git(), Some(s.bare.clone()))
}

fn env(kind: Option<&str>) -> HookEnv {
    HookEnv::new(kind, false, None)
}

fn update(tip: &str, name: &str) -> RefUpdate {
    RefUpdate::parse_line(&format!("{ZERO} {tip} {name}")).unwrap()
}

/// `"<ref> <path> <code> <rule>"` per rejection, in order.
fn rows(r: &[Rejection]) -> Vec<String> {
    r.iter()
        .map(|x| {
            format!(
                "{} {} {} {}",
                x.ref_name(),
                String::from_utf8_lossy(x.path()),
                x.finding().code().as_str(),
                x.finding().rule()
            )
        })
        .collect()
}

fn scan_main(s: &Scratch, env: &HookEnv, tip: &str) -> Vec<String> {
    rows(&scan(env, &[update(tip, "refs/heads/main")], &git_for(s)).unwrap())
}

#[test]
fn a_clean_push_is_accepted_and_a_root_commit_with_a_secret_is_not() {
    let s = Scratch::new();
    s.write("README", b"hello\n");
    let clean = s.stage_commit("one");
    assert!(scan_main(&s, &env(Some("apinata")), &clean).is_empty());
    s.write(".env", b"KEY=fixture\n");
    let root = s.stage_commit("root with env");
    assert_eq!(
        scan_main(&s, &env(None), &root),
        ["refs/heads/main .env secret_env_file env_file"],
        "the root commit's whole tree counts"
    );
}

#[test]
fn a_secret_only_in_a_merge_commit_is_refused() {
    let s = Scratch::new();
    s.write("README", b"seed\n");
    s.commit("seed");
    s.publish("main");
    s.branch("feat");
    s.write("feat.txt", b"f\n");
    s.commit("feat");
    s.checkout("main");
    s.write("main.txt", b"m\n");
    s.commit("main");
    s.merge_no_commit("feat");
    s.write("creds.txt", AWS);
    let tip = s.stage_commit("evil merge");
    assert_eq!(
        scan_main(&s, &env(None), &tip),
        ["refs/heads/main creds.txt secret_pattern aws"]
    );
}

#[test]
fn a_secret_added_then_removed_in_one_push_is_refused_but_passes_as_an_import() {
    let s = Scratch::new();
    s.write("README", b"seed\n");
    s.commit("seed");
    s.publish("main");
    s.write("creds.txt", AWS);
    s.commit("add");
    s.remove("creds.txt");
    let tip = s.stage_commit("remove");
    assert_eq!(
        scan_main(&s, &env(None), &tip),
        ["refs/heads/main creds.txt secret_pattern aws"]
    );
    assert!(
        scan_main(&s, &HookEnv::new(None, true, None), &tip).is_empty(),
        "import mode sees the clean tip tree only"
    );
}

#[test]
fn import_mode_still_refuses_a_secret_in_the_tip_tree() {
    let s = Scratch::new();
    s.write(".env", b"KEY=fixture\n");
    s.write("environments/dev.yml", ENV_YML);
    let tip = s.stage_commit("imported");
    assert_eq!(
        scan_main(&s, &HookEnv::new(Some("apinata"), true, None), &tip),
        [
            "refs/heads/main .env secret_env_file env_file",
            "refs/heads/main environments/dev.yml secret_value secret_value",
        ]
    );
}

#[test]
fn what_the_server_already_has_is_not_rescanned() {
    let s = Scratch::new();
    s.write(".env", b"KEY=fixture\n");
    s.write("creds.txt", AWS);
    s.commit("already there");
    s.publish("main");
    s.write("README", b"later\n");
    let tip = s.stage_commit("later");
    assert!(scan_main(&s, &env(Some("apinata")), &tip).is_empty());
}

#[test]
fn kind_rules_follow_the_kind_and_the_generic_set_runs_for_all() {
    let s = Scratch::new();
    s.write("environments/dev.yml", ENV_YML);
    s.write(".env", b"x\n");
    let tip = s.stage_commit("env");
    assert_eq!(
        scan_main(&s, &env(Some("apinata")), &tip),
        [
            "refs/heads/main .env secret_env_file env_file",
            "refs/heads/main environments/dev.yml secret_value secret_value",
        ]
    );
    for kind in [None, Some("dbento")] {
        assert_eq!(
            scan_main(&s, &env(kind), &tip),
            ["refs/heads/main .env secret_env_file env_file"],
            "{kind:?}: generic only"
        );
    }
}

#[test]
fn a_commit_shared_by_two_refs_is_reported_under_the_first() {
    let s = Scratch::new();
    s.write("creds.txt", AWS);
    s.commit("secret");
    s.branch("feat");
    s.write("f.txt", b"f\n");
    s.commit("feat");
    let feat = s.stage("feat");
    let main = s.stage("main");
    let git = git_for(&s);
    let r = scan(
        &env(None),
        &[
            update(&main, "refs/heads/main"),
            update(&feat, "refs/heads/feat"),
        ],
        &git,
    )
    .unwrap();
    assert_eq!(rows(&r), ["refs/heads/main creds.txt secret_pattern aws"]);
    let r = scan(
        &env(None),
        &[
            update(&feat, "refs/heads/feat"),
            update(&main, "refs/heads/main"),
        ],
        &git,
    )
    .unwrap();
    assert_eq!(rows(&r), ["refs/heads/feat creds.txt secret_pattern aws"]);
}

#[test]
fn a_ref_to_a_tree_or_a_blob_is_scanned_too() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    s.commit("seed");
    s.publish("main");
    s.branch("leak-src");
    s.write("creds.txt", AWS);
    s.write("sub/.env", b"x\n");
    s.commit("never pushed as a commit");
    s.tag("blob-leak", &s.blob("leak-src", "creds.txt"));
    s.tag("tree-leak", &s.work_git(&["rev-parse", "leak-src^{tree}"]));
    let blob_tag = s.stage("refs/tags/blob-leak");
    let tree_tag = s.stage("refs/tags/tree-leak");
    let git = git_for(&s);
    assert_eq!(
        rows(
            &scan(
                &env(None),
                &[update(&blob_tag, "refs/tags/blob-leak")],
                &git
            )
            .unwrap()
        ),
        ["refs/tags/blob-leak - secret_pattern aws"]
    );
    assert_eq!(
        rows(
            &scan(
                &env(None),
                &[update(&tree_tag, "refs/tags/tree-leak")],
                &git
            )
            .unwrap()
        ),
        [
            "refs/tags/tree-leak sub/.env secret_env_file env_file",
            "refs/tags/tree-leak creds.txt secret_pattern aws",
        ]
    );
}

#[test]
fn deletions_and_overrides_touch_no_git_and_an_oversized_blob_skips_content_rules() {
    let s = Scratch::new();
    s.write("creds.txt", AWS);
    let tip = s.stage_commit("secret");
    let broken = Git::new(PathBuf::from("/nonexistent/git"), None);
    let delete = RefUpdate::parse_line(&format!("{tip} {ZERO} refs/heads/main")).unwrap();
    assert!(scan(&env(None), &[delete], &broken).unwrap().is_empty());
    assert!(
        scan(
            &HookEnv::new(Some("apinata"), false, Some("17")),
            &[update(&tip, "refs/heads/main")],
            &broken
        )
        .unwrap()
        .is_empty()
    );
    // The secret commit becomes server history, so the next push is judged on the big blob alone.
    s.publish("main");
    let mut big = vec![b'a'; 1024 * 1024];
    big.extend_from_slice(b"\nAKIAIOSFODNN7EXAMPLE\n");
    s.write("big.txt", &big);
    s.write("sub/.env", &big);
    let tip = s.stage_commit("big");
    assert_eq!(
        scan_main(&s, &env(None), &tip),
        ["refs/heads/main sub/.env secret_env_file env_file"],
        "path rule yes, content rule skipped above the cap"
    );
}

#[test]
fn a_git_failure_is_an_error() {
    let s = Scratch::new();
    let bad = Git::new(s.isolated_git(), Some(s.dir.path().join("nope.git")));
    let err = scan(
        &env(None),
        &[update(&"a".repeat(40), "refs/heads/main")],
        &bad,
    )
    .unwrap_err();
    assert!(matches!(err, ScanError::Git(_)), "{err:?}");
}

#[test]
fn a_blob_new_to_the_repository_is_checked_under_every_ref_that_brings_it() {
    let s = Scratch::new();
    s.write("notes/dev.yml", ENV_YML);
    s.commit("notes");
    s.branch("feat");
    s.write("environments/dev.yml", ENV_YML);
    s.commit("env");
    let feat = s.stage("feat");
    let main = s.stage("main");
    let r = scan(
        &env(Some("apinata")),
        &[
            update(&main, "refs/heads/main"),
            update(&feat, "refs/heads/feat"),
        ],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(
        rows(&r),
        ["refs/heads/feat environments/dev.yml secret_value secret_value"],
        "the earlier ref's copy of the blob must not hide the later path"
    );
}

#[test]
fn a_candidate_blob_with_no_size_is_an_error() {
    let s = Scratch::new();
    s.write("creds.txt", AWS);
    let tip = s.stage_commit("secret");
    // Real git answers every lookup; this one hides blobs from `--batch-check`, keeping peel's commit answer.
    let iso = s.isolated_git();
    let lossy = s.script(
        "blobless-git",
        &format!(
            "if [ \"$1\" = cat-file ] && [ \"$2\" = --batch-check ]; then '{}' \"$@\" | sed '/ blob /d'; exit 0; fi\nexec '{}' \"$@\"\n",
            iso.display(),
            iso.display()
        ),
    );
    let git = Git::new(lossy, Some(s.bare.clone()));
    let err = scan(&env(None), &[update(&tip, "refs/heads/main")], &git).unwrap_err();
    assert!(matches!(err, ScanError::Git(_)), "{err:?}");
}
