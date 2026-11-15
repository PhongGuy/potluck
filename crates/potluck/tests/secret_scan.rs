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
        ["refs/tags/blob-leak / secret_pattern aws"]
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

/// Potluck's own token shape: `plk_` and 43 base64url characters (generic rule `potluck`).
fn plk() -> String {
    format!("plk_{}", "0".repeat(43))
}

#[test]
fn a_token_in_a_commit_message_is_refused_at_slash_after_the_content_hits() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    let clean = s.stage_commit("a clean message");
    assert!(scan_main(&s, &env(Some("apinata")), &clean).is_empty());
    s.write(".env", b"x\n");
    let tip = s.stage_commit(&format!("token {}", plk()));
    assert_eq!(
        scan_main(&s, &env(None), &tip),
        [
            "refs/heads/main .env secret_env_file env_file",
            "refs/heads/main / secret_pattern potluck",
        ]
    );
    assert_eq!(
        scan_main(&s, &HookEnv::new(None, true, None), &tip),
        ["refs/heads/main .env secret_env_file env_file"],
        "import mode scans the tip tree only, no message"
    );
}

#[test]
fn a_token_in_an_annotated_tag_message_is_refused_under_the_tag() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    s.commit("seed");
    s.publish("main");
    s.tag_message("v1", "main", &format!("release {}", plk()));
    let tag = s.stage("refs/tags/v1");
    let git = git_for(&s);
    assert_eq!(
        rows(&scan(&env(None), &[update(&tag, "refs/tags/v1")], &git).unwrap()),
        ["refs/tags/v1 / secret_pattern potluck"],
        "the tagged commit is old; the tag is what is new"
    );
    s.tag_message("v1-copy", "refs/tags/v1", "clean outer tag");
    let outer = s.stage("refs/tags/v1-copy");
    assert_eq!(
        rows(
            &scan(
                &env(None),
                &[
                    update(&outer, "refs/tags/v1-copy"),
                    update(&tag, "refs/tags/v1"),
                ],
                &git,
            )
            .unwrap()
        ),
        ["refs/tags/v1-copy / secret_pattern potluck"],
        "a nested tag's inner message is scanned once, under the first ref that brings it"
    );
}

#[test]
fn a_tag_of_a_blob_reports_its_target_and_its_message_both_at_slash() {
    let s = Scratch::new();
    s.write("creds.txt", AWS);
    s.commit("source");
    let blob = s.blob("main", "creds.txt");
    s.tag_message("leak", &blob, &format!("see {}", plk()));
    let tag = s.stage("refs/tags/leak");
    assert_eq!(
        rows(&scan(&env(None), &[update(&tag, "refs/tags/leak")], &git_for(&s)).unwrap()),
        [
            "refs/tags/leak / secret_pattern aws",
            "refs/tags/leak / secret_pattern potluck",
        ],
        "the content hit first, then the message"
    );
}

#[test]
fn a_message_over_the_cap_is_skipped_like_a_blob() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    s.commit("seed");
    let big = s.dir.path().join("big-message");
    let mut text = vec![b'a'; 1024 * 1024];
    text.extend_from_slice(format!("\n{}\n", plk()).as_bytes());
    std::fs::write(&big, &text).unwrap();
    s.git(
        Some(&s.work),
        &["commit", "-q", "--allow-empty", "-F", big.to_str().unwrap()],
    );
    let tip = s.stage("main");
    assert!(scan_main(&s, &env(None), &tip).is_empty());
}

/// An isolated git that appends every name fed to `cat-file --batch` to `log`, so a test can
/// say which blobs were read.
fn logging_git(s: &Scratch, log: &std::path::Path) -> Git {
    let iso = s.isolated_git();
    let script = s.script(
        "logging-git",
        &format!(
            "if [ \"$1\" = cat-file ] && [ \"$2\" = --batch ]; then tee -a '{}' | '{}' \"$@\"; exit $?; fi\nexec '{}' \"$@\"\n",
            log.display(),
            iso.display(),
            iso.display()
        ),
    );
    Git::new(script, Some(s.bare.clone()))
}

fn logged(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .map(|t| t.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

#[test]
fn an_old_blob_moved_under_environments_is_checked_there_and_only_there() {
    let s = Scratch::new();
    s.write("notes/x.txt", ENV_YML);
    s.write("README", b"seed\n");
    let first = s.stage_commit("notes");
    assert!(
        scan_main(&s, &env(Some("apinata")), &first).is_empty(),
        "under notes/ only the generic set applies"
    );
    s.publish("main");
    // A move and a copy, both of blobs the server already has.
    s.remove("notes/x.txt");
    s.write("environments/dev.yml", ENV_YML);
    s.write("docs/README", b"seed\n");
    let tip = s.stage_commit("move");
    let log = s.dir.path().join("batch-names");
    let git = logging_git(&s, &log);
    let main = update(&tip, "refs/heads/main");
    assert_eq!(
        rows(&scan(&env(Some("apinata")), std::slice::from_ref(&main), &git).unwrap()),
        ["refs/heads/main environments/dev.yml secret_value secret_value"]
    );
    // The log also carries the commit the message pass reads, so look for the blobs by name.
    let env_blob = s.blob("main", "environments/dev.yml");
    let readme_blob = s.blob("main", "docs/README");
    assert!(
        rows(&scan(&env(None), &[main], &git).unwrap()).is_empty(),
        "without a kind an old blob is nobody's candidate"
    );
    let read = logged(&log);
    assert_eq!(
        read.iter().filter(|name| **name == env_blob).count(),
        1,
        "read once, under the kind, and not without it: {read:?}"
    );
    assert!(
        !read.contains(&readme_blob),
        "the README copy under docs/ is never read: {read:?}"
    );
}

#[test]
fn a_rename_inside_environments_rechecks_a_clean_file_and_passes() {
    let s = Scratch::new();
    s.write("environments/dev.yml", b"name: dev\n");
    s.commit("env");
    s.publish("main");
    s.remove("environments/dev.yml");
    s.write("environments/prod.yml", b"name: dev\n");
    let tip = s.stage_commit("rename");
    let log = s.dir.path().join("batch-names");
    let git = logging_git(&s, &log);
    assert!(
        rows(
            &scan(
                &env(Some("apinata")),
                &[update(&tip, "refs/heads/main")],
                &git
            )
            .unwrap()
        )
        .is_empty()
    );
    let read = logged(&log);
    assert!(
        read.contains(&s.blob("main", "environments/prod.yml")),
        "the old blob is read under its new path: {read:?}"
    );
}

#[test]
fn the_same_blob_and_path_pushed_under_two_refs_is_reported_once() {
    let s = Scratch::new();
    s.write("README", b"seed\n");
    s.commit("seed");
    s.publish("main");
    s.branch("a");
    s.write("environments/dev.yml", ENV_YML);
    s.commit("a");
    s.checkout("main");
    s.branch("b");
    s.write("environments/dev.yml", ENV_YML);
    s.write("b.txt", b"b\n");
    s.commit("b");
    let a = s.stage("a");
    let b = s.stage("b");
    assert_eq!(
        rows(
            &scan(
                &env(Some("apinata")),
                &[update(&a, "refs/heads/a"), update(&b, "refs/heads/b")],
                &git_for(&s),
            )
            .unwrap()
        ),
        ["refs/heads/a environments/dev.yml secret_value secret_value"],
        "two distinct commits, one (blob, path) pair: the first ref reports it"
    );
}

#[test]
fn an_environment_file_over_the_cap_is_refused_as_unparsable_and_other_big_files_pass() {
    let s = Scratch::new();
    let big = vec![b'a'; 1024 * 1024 + 1];
    s.write("environments/big.yml", &big);
    s.write("notes/big.bin", &big);
    s.write("README", b"r\n");
    let tip = s.stage_commit("big");
    assert_eq!(
        scan_main(&s, &env(Some("apinata")), &tip),
        ["refs/heads/main environments/big.yml secret_value env_unparsable"],
        "the same blob at notes/ is not the kind's and stays skipped"
    );
    assert!(
        scan_main(&s, &env(None), &tip).is_empty(),
        "the generic set never reads over-cap content"
    );
    assert!(
        scan_main(&s, &HookEnv::new(Some("apinata"), true, None), &tip).contains(
            &"refs/heads/main environments/big.yml secret_value env_unparsable".to_owned()
        ),
        "import mode scans the tip tree and finds it too"
    );
}
