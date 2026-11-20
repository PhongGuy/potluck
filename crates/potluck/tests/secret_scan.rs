//! The whole check over staged pushes.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use potluck::role::Role;
use potluck::secret_check::git::{Git, RefUpdate};
use potluck::secret_check::scan::{HookEnv, Rejection, ScanError, report, scan, scan_with};

use crate::common::APINATA_FORMAT;
use crate::common::scratch::Scratch;
use crate::test_rules::{Act, TestRules, rules};

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
    RefUpdate::parse_line(format!("{ZERO} {tip} {name}").as_bytes()).unwrap()
}

/// `"<ref> <path> <code> <rule>"` per rejection, in order.
fn rows(r: &[Rejection]) -> Vec<String> {
    r.iter()
        .map(|x| {
            format!(
                "{} {} {} {}",
                String::from_utf8_lossy(x.ref_name()),
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
    s.write("opencollection.yml", APINATA_FORMAT);
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
    s.write("opencollection.yml", APINATA_FORMAT);
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
    s.write("opencollection.yml", APINATA_FORMAT);
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
    s.write("opencollection.yml", APINATA_FORMAT);
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
    assert_eq!(
        scan_main(&s, &env(None), &tip),
        ["refs/heads/main .env secret_env_file env_file"],
        "None: generic only"
    );
}

#[test]
fn a_token_in_an_environment_file_with_a_valued_secret_reports_the_generic_rule() {
    // Both rule sets would refuse this file; the generic pattern runs first and the kind's
    // check is asked only when it finds nothing.
    let s = Scratch::new();
    s.write("opencollection.yml", APINATA_FORMAT);
    let both = [ENV_YML, b"# AKIAIOSFODNN7EXAMPLE\n"].concat();
    let kind = potluck::kinds::by_name("apinata").expect("apinata is registered");
    let found = kind.check(&potluck_kind::Change::history(
        "environments/dev.yml",
        potluck_kind::Blob::Bytes(&both),
        potluck_kind::Role::Write,
    ));
    assert_eq!(
        found.iter().map(|f| f.rule()).collect::<Vec<_>>(),
        ["secret_value"],
        "the kind's rule must refuse this file too, or the test proves nothing"
    );
    s.write("environments/dev.yml", &both);
    let tip = s.stage_commit("token and valued secret");
    assert_eq!(
        scan_main(&s, &env(Some("apinata")), &tip),
        ["refs/heads/main environments/dev.yml secret_pattern aws"]
    );
}

#[test]
fn every_kind_accepts_a_new_workspace_holding_its_own_seed() {
    // A created ref runs the path, history and format passes over exactly the seed.
    for k in potluck::kinds::all() {
        let s = Scratch::new();
        for f in k.seed() {
            s.write(f.path(), f.bytes());
        }
        let tip = s.stage_commit("seed");
        assert!(
            scan_main(&s, &env(Some(k.kind())), &tip).is_empty(),
            "{} refuses its own seed",
            k.kind()
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
    let delete = RefUpdate::parse_line(format!("{tip} {ZERO} refs/heads/main").as_bytes()).unwrap();
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
    s.write("opencollection.yml", APINATA_FORMAT);
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
    s.write("opencollection.yml", APINATA_FORMAT);
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
    s.write("opencollection.yml", APINATA_FORMAT);
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
    s.write("opencollection.yml", APINATA_FORMAT);
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
    s.write("opencollection.yml", APINATA_FORMAT);
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
    s.write("opencollection.yml", APINATA_FORMAT);
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

#[test]
fn an_old_blob_over_the_cap_moved_under_environments_is_refused_as_unparsable() {
    let s = Scratch::new();
    s.write("opencollection.yml", APINATA_FORMAT);
    let big = vec![b'a'; 1024 * 1024 + 1];
    s.write("notes/big.bin", &big);
    let first = s.stage_commit("big note");
    assert!(
        scan_main(&s, &env(Some("apinata")), &first).is_empty(),
        "under notes/ an over-cap blob is skipped"
    );
    s.publish("main");
    s.remove("notes/big.bin");
    s.write("environments/big.yml", &big);
    let tip = s.stage_commit("move");
    assert_eq!(
        scan_main(&s, &env(Some("apinata")), &tip),
        ["refs/heads/main environments/big.yml secret_value env_unparsable"],
        "the blob the server already holds is refused at the kind's path"
    );
}

fn scan_rules(s: &Scratch, r: &TestRules, tip: &str) -> Result<Vec<String>, ScanError> {
    scan_with(
        &env(None),
        Some(r),
        &[update(tip, "refs/heads/main")],
        &git_for(s),
    )
    .map(|x| rows(&x))
}

#[test]
fn a_panicking_or_misnaming_or_mismatched_rule_set_fails_closed() {
    let s = Scratch::new();
    s.write("a.t", b"x\n");
    let tip = s.stage_commit("t");
    for r in [
        rules(Act::Panic),
        rules(Act::PanicInPathRule),
        rules(Act::BadRuleName),
    ] {
        assert!(
            matches!(scan_rules(&s, &r, &tip), Err(ScanError::Rules)),
            "{r:?}"
        );
    }
    let wrong = TestRules {
        act: Act::Detail("k"),
        abi: potluck::kinds::RULES_ABI + 1,
    };
    assert!(matches!(
        scan_rules(&s, &wrong, &tip),
        Err(ScanError::Rules)
    ));
}

#[test]
fn a_finding_at_another_path_reports_that_path() {
    let s = Scratch::new();
    s.write("a.t", b"x\n");
    let tip = s.stage_commit("t");
    assert_eq!(
        scan_rules(&s, &rules(Act::Elsewhere), &tip).unwrap(),
        ["refs/heads/main other/place.t secret_value elsewhere"]
    );
}

#[test]
fn a_rule_set_finding_at_a_non_utf8_path_keeps_the_raw_bytes() {
    let s = Scratch::new();
    std::fs::write(s.work.join(OsStr::from_bytes(b"caf\xe9.t")), b"x\n").unwrap();
    let tip = s.stage_commit("latin-1 name");
    let found = scan_with(
        &env(None),
        Some(&rules(Act::Plain)),
        &[update(&tip, "refs/heads/main")],
        &git_for(&s),
    )
    .unwrap();
    let mut out = Vec::new();
    report(&found, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "potluck-reject secret_value refs/heads/main caf%E9.t plain\n"
    );
}

#[test]
fn a_generic_path_finding_at_a_non_utf8_path_keeps_the_raw_bytes() {
    // The generic `.env` rule matches on the lossy path, but the client must get back the
    // exact bytes git stores, so the report escapes the Latin-1 byte rather than U+FFFD.
    let s = Scratch::new();
    let dir = s.work.join(OsStr::from_bytes(b"caf\xe9"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".env"), b"KEY=fixture\n").unwrap();
    let tip = s.stage_commit("latin-1 directory");
    let found = scan(&env(None), &[update(&tip, "refs/heads/main")], &git_for(&s)).unwrap();
    let mut out = Vec::new();
    report(&found, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "potluck-reject secret_env_file refs/heads/main caf%E9/.env env_file\n"
    );
}

#[test]
fn an_unknown_kind_is_refused_unless_every_update_is_a_delete() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    let tip = s.stage_commit("r");
    let res = scan(
        &env(Some("no-such-kind")),
        &[update(&tip, "refs/heads/main")],
        &git_for(&s),
    );
    assert!(matches!(res, Err(ScanError::UnsupportedKind)));
    let delete = RefUpdate::parse_line(format!("{tip} {ZERO} refs/heads/gone").as_bytes()).unwrap();
    assert!(
        scan(&env(Some("no-such-kind")), &[delete], &git_for(&s))
            .unwrap()
            .is_empty()
    );
}

/// Re-runs this test binary on `panic_child` with the quiet hook installed, and checks that the
/// child's stderr never shows the panic message.
#[test]
fn a_rule_panic_prints_nothing_with_quiet_panics() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "secret_scan::panic_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("POTLUCK_TEST_PANIC_CHILD", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.contains("1 passed"), "the child ran: {stdout}");
    assert!(!stderr.contains("PANIC-SECRET-TEXT"), "{stderr}");
}

#[test]
fn panic_child() {
    if std::env::var_os("POTLUCK_TEST_PANIC_CHILD").is_none() {
        return;
    }
    potluck::secret_check::quiet_panics();
    let s = Scratch::new();
    s.write("a.t", b"x\n");
    let tip = s.stage_commit("t");
    assert!(matches!(
        scan_rules(&s, &rules(Act::Panic), &tip),
        Err(ScanError::Rules)
    ));
}

/// Re-runs this test binary on `role_child` once per `POTLUCK_ROLE` value, since the hook reads
/// its role from the process environment.
#[test]
fn the_hook_reads_its_role_from_potluck_role_and_falls_back_to_write() {
    let cases: [(Option<&str>, &str); 7] = [
        (None, "write"),
        (Some(""), "write"),
        (Some("admin"), "admin"),
        (Some("write"), "write"),
        (Some("read"), "read"),
        (Some("Admin"), "write"),
        (Some("owner"), "write"),
    ];
    for (value, expected) in cases {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "secret_scan::role_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("POTLUCK_TEST_ROLE_CHILD", expected)
        .env_remove("POTLUCK_ROLE");
        if let Some(v) = value {
            cmd.env("POTLUCK_ROLE", v);
        }
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{value:?}: {stdout}{stderr}");
        assert!(
            stdout.contains("1 passed"),
            "{value:?}: the child ran: {stdout}"
        );
    }
}

#[test]
fn role_child() {
    let Some(expected) = std::env::var_os("POTLUCK_TEST_ROLE_CHILD") else {
        return;
    };
    assert_eq!(
        HookEnv::from_env().role().as_str(),
        expected.to_str().unwrap()
    );
}

#[test]
fn a_panic_in_reads_fails_closed() {
    // A blob the server already has is content-checked at a new path only if the kind reads it,
    // so copying a published `.t` file is what makes the scan ask `reads`.
    let s = Scratch::new();
    s.write("a.t", b"x\n");
    s.commit("a");
    s.publish("main");
    s.write("b.t", b"x\n");
    let tip = s.stage_commit("copy");
    assert!(matches!(
        scan_rules(&s, &rules(Act::PanicInReads), &tip),
        Err(ScanError::Rules)
    ));
}

#[test]
fn a_misnamed_path_rule_finding_fails_closed() {
    let s = Scratch::new();
    s.write("a.t", b"x\n");
    let tip = s.stage_commit("t");
    assert!(matches!(
        scan_rules(&s, &rules(Act::BadPathRuleName), &tip),
        Err(ScanError::Rules)
    ));
}

#[test]
fn a_malformed_finding_after_a_valid_one_fails_closed() {
    let s = Scratch::new();
    s.write("a.t", b"x\n");
    let tip = s.stage_commit("t");
    assert!(matches!(
        scan_rules(&s, &rules(Act::BadLaterFinding), &tip),
        Err(ScanError::Rules)
    ));
}

#[test]
fn an_empty_rule_name_fails_closed() {
    let s = Scratch::new();
    s.write("a.t", b"x\n");
    let tip = s.stage_commit("t");
    assert!(matches!(
        scan_rules(&s, &rules(Act::EmptyRuleName), &tip),
        Err(ScanError::Rules)
    ));
}

#[test]
fn a_rule_set_detail_reaches_the_rejection() {
    let s = Scratch::new();
    s.write("a.t", b"x\n");
    let tip = s.stage_commit("t");
    let r = scan_with(
        &env(None),
        Some(&rules(Act::Detail("key.path"))),
        &[update(&tip, "refs/heads/main")],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(r[0].finding().detail(), Some("key.path"));
}

fn moved(old: &str, new: &str, name: &str) -> RefUpdate {
    RefUpdate::parse_line(format!("{old} {new} {name}").as_bytes()).unwrap()
}

fn relax_rows(s: &Scratch, role: Role, u: RefUpdate) -> Vec<String> {
    let e = HookEnv::new(None, false, None).with_role(role);
    rows(&scan_with(&e, Some(&rules(Act::Relax)), &[u], &git_for(s)).unwrap())
}

const RELAXED: &[&str] = &["refs/heads/main a.t safety_relaxation relax_test"];

#[test]
fn a_relaxing_update_is_refused_for_write_and_passes_for_admin() {
    let s = Scratch::new();
    s.write("a.t", b"strict\n");
    let old = s.commit("strict");
    s.publish("main");
    s.write("a.t", b"relaxed\n");
    let new = s.stage_commit("relax");
    assert_eq!(
        relax_rows(&s, Role::Write, moved(&old, &new, "refs/heads/main")),
        RELAXED
    );
    assert_eq!(
        relax_rows(&s, Role::Read, moved(&old, &new, "refs/heads/main")),
        RELAXED
    );
    assert!(relax_rows(&s, Role::Admin, moved(&old, &new, "refs/heads/main")).is_empty());
}

#[test]
fn a_history_pass_relaxation_is_refused_for_write_and_dropped_for_admin() {
    let s = Scratch::new();
    s.write("a.t", b"x\n");
    let tip = s.stage_commit("created with a relaxation");
    let scan_as = |role: Role| {
        let e = HookEnv::new(None, false, None).with_role(role);
        rows(
            &scan_with(
                &e,
                Some(&rules(Act::RelaxAlways)),
                &[update(&tip, "refs/heads/main")],
                &git_for(&s),
            )
            .unwrap(),
        )
    };
    assert_eq!(
        scan_as(Role::Write),
        ["refs/heads/main a.t safety_relaxation relax_always"],
        "a created ref gets no tip pass, so this is the history pass"
    );
    assert!(scan_as(Role::Admin).is_empty());
}

#[test]
fn an_import_update_still_gets_the_tip_pass() {
    let s = Scratch::new();
    s.write("a.t", b"strict\n");
    let old = s.commit("strict");
    s.publish("main");
    s.write("a.t", b"relaxed\n");
    let new = s.stage_commit("relax");
    let import = HookEnv::new(None, true, None);
    assert_eq!(
        rows(
            &scan_with(
                &import,
                Some(&rules(Act::Relax)),
                &[moved(&old, &new, "refs/heads/main")],
                &git_for(&s),
            )
            .unwrap()
        ),
        RELAXED
    );
}

#[test]
fn a_created_ref_has_no_baseline_and_a_delete_is_not_checked() {
    let s = Scratch::new();
    s.write("a.t", b"relaxed\n");
    let tip = s.stage_commit("relaxed from the start");
    assert!(relax_rows(&s, Role::Write, update(&tip, "refs/heads/main")).is_empty());
    assert!(relax_rows(&s, Role::Write, moved(&tip, ZERO, "refs/heads/main")).is_empty());
}

#[test]
fn a_relax_then_restore_in_one_push_is_judged_by_its_net_result() {
    let s = Scratch::new();
    s.write("a.t", b"strict\n");
    let old = s.commit("strict");
    s.publish("main");
    s.write("a.t", b"relaxed\n");
    s.commit("relax");
    s.write("a.t", b"strict\n");
    let new = s.stage_commit("restore");
    assert!(relax_rows(&s, Role::Write, moved(&old, &new, "refs/heads/main")).is_empty());
}

#[test]
fn a_force_push_to_unrelated_history_is_judged_old_against_new() {
    let s = Scratch::new();
    s.write("a.t", b"strict\n");
    let old = s.commit("strict");
    s.publish("main");
    s.git(Some(&s.work), &["checkout", "-q", "--orphan", "other"]);
    s.write("a.t", b"relaxed\n");
    s.commit("unrelated");
    let new = s.stage("other");
    assert_eq!(
        relax_rows(&s, Role::Write, moved(&old, &new, "refs/heads/main")),
        RELAXED
    );
}

#[test]
fn a_fast_forward_onto_another_refs_commits_is_re_judged() {
    let s = Scratch::new();
    s.write("opencollection.yml", b"opencollection: 1.0.0\n");
    let old = s.commit("seed");
    s.publish("main");
    s.branch("feat");
    s.write("environments/dev.yml", ENV_YML);
    let feat = s.commit("accepted on feat under an override");
    s.publish("feat");
    assert_eq!(
        scan_main_moved(&s, &old, &feat),
        ["refs/heads/main environments/dev.yml secret_value secret_value"],
        "no new commits, so only the tip pass sees it"
    );
}

fn scan_main_moved(s: &Scratch, old: &str, new: &str) -> Vec<String> {
    rows(
        &scan(
            &env(Some("apinata")),
            &[moved(old, new, "refs/heads/main")],
            &git_for(s),
        )
        .unwrap(),
    )
}

#[test]
fn a_tip_finding_at_a_path_another_ref_already_reported_prints_once() {
    let s = Scratch::new();
    s.write("opencollection.yml", b"opencollection: 1.0.0\n");
    let old = s.commit("seed");
    s.publish("main");
    s.write("environments/dev.yml", ENV_YML);
    let tip = s.stage_commit("secret");
    let both = [
        update(&tip, "refs/heads/feat"),
        moved(&old, &tip, "refs/heads/main"),
    ];
    assert_eq!(
        rows(&scan(&env(Some("apinata")), &both, &git_for(&s)).unwrap()),
        ["refs/heads/feat environments/dev.yml secret_value secret_value"],
        "feat brings the commit; main's tip finding at the same path is not repeated"
    );
}

#[test]
fn a_tip_finding_already_reported_by_the_history_pass_prints_once() {
    let s = Scratch::new();
    s.write("opencollection.yml", b"opencollection: 1.0.0\n");
    let old = s.commit("seed");
    s.publish("main");
    s.write("environments/dev.yml", ENV_YML);
    let new = s.stage_commit("secret");
    assert_eq!(
        scan_main_moved(&s, &old, &new),
        ["refs/heads/main environments/dev.yml secret_value secret_value"]
    );
}

fn fmt_rows(s: &Scratch, u: RefUpdate) -> Vec<String> {
    rows(&scan(&env(Some("apinata")), &[u], &git_for(s)).unwrap())
}

const UNSUPPORTED: &[&str] =
    &["refs/heads/main opencollection.yml format_unsupported format_unsupported"];

#[test]
fn a_created_ref_without_a_listed_format_is_refused() {
    for file in [
        None,
        Some(&b"opencollection: 9.9.9\n"[..]),
        Some(b"name: x\n"),
    ] {
        let s = Scratch::new();
        s.write("README", b"r\n");
        if let Some(bytes) = file {
            s.write("opencollection.yml", bytes);
        }
        let tip = s.stage_commit("c");
        assert_eq!(
            fmt_rows(&s, update(&tip, "refs/heads/main")),
            UNSUPPORTED,
            "{file:?}"
        );
    }
    let s = Scratch::new();
    s.write("opencollection.yml/inner", b"x\n");
    let tip = s.stage_commit("a directory, not a file");
    assert_eq!(fmt_rows(&s, update(&tip, "refs/heads/main")), UNSUPPORTED);
}

#[test]
fn an_update_is_re_judged_only_when_it_touches_the_format_file() {
    let s = Scratch::new();
    s.write("opencollection.yml", b"opencollection: 0.9.0\n");
    let old = s.commit("old format, accepted before");
    s.publish("main");
    s.write("README", b"r\n");
    let untouched = s.stage_commit("leaves the file alone");
    assert!(fmt_rows(&s, moved(&old, &untouched, "refs/heads/main")).is_empty());
    s.write("opencollection.yml", b"opencollection: \"1.0.0\"\n");
    let migrated = s.stage_commit("migrates");
    assert!(fmt_rows(&s, moved(&old, &migrated, "refs/heads/main")).is_empty());
    s.write("opencollection.yml", b"opencollection: 2.0.0\n");
    let ahead = s.stage_commit("too new");
    assert_eq!(
        fmt_rows(&s, moved(&old, &ahead, "refs/heads/main")),
        UNSUPPORTED
    );
}

#[test]
fn without_a_kind_in_the_env_no_format_pass_runs() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    let tip = s.stage_commit("c");
    assert!(scan_main(&s, &env(None), &tip).is_empty());
    let injected = scan_with(
        &env(None),
        Some(&rules(Act::Relax)),
        &[update(&tip, "refs/heads/main")],
        &git_for(&s),
    )
    .unwrap();
    assert!(
        injected.is_empty(),
        "an injected rule set with no kind env is not format-checked"
    );
}

#[test]
fn accepted_relaxations_reports_only_the_relaxing_updates() {
    let s = Scratch::new();
    s.write("a.t", b"strict\n");
    let old = s.commit("strict");
    s.publish("main");
    s.write("a.t", b"relaxed\n");
    let new = s.commit("relax");
    s.publish("main");
    let got = potluck::secret_check::scan::accepted_relaxations(
        &rules(Act::Relax),
        &[
            moved(&old, &new, "refs/heads/main"),
            update(&new, "refs/heads/copy"),
        ],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&got), RELAXED);
}

#[test]
fn an_accepted_relaxation_is_logged_with_workspace_ref_user_and_rule() {
    let s = Scratch::new();
    s.write("a.t", b"strict\n");
    let old = s.commit("strict");
    s.publish("main");
    s.write("a.t", b"relaxed\n");
    let new = s.commit("relax");
    s.publish("main");
    let text = crate::logs::captured_logs(|| {
        potluck::secret_check::log_accepted_relaxations(
            7,
            "alice",
            &rules(Act::Relax),
            &[moved(&old, &new, "refs/heads/main")],
            &git_for(&s),
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
        "relax_test",
    ] {
        assert!(line.contains(needle), "{needle} in {line}");
    }
    let broken = Git::new(PathBuf::from("/nonexistent/git"), Some(s.bare.clone()));
    let text = crate::logs::captured_logs(|| {
        potluck::secret_check::log_accepted_relaxations(
            7,
            "alice",
            &rules(Act::Relax),
            &[moved(&old, &new, "refs/heads/main")],
            &broken,
        );
    });
    assert!(
        text.contains(" WARN ") && text.contains("could not re-check an accepted admin push"),
        "{text}"
    );
}

/// A `size`-byte body that starts with `head` and is unique per `seed`, so no two sides share
/// a blob.
fn big_body(head: &[u8], seed: usize, size: u64) -> Vec<u8> {
    let mut body = head.to_vec();
    let fill = format!(" {seed:04} ");
    while (body.len() as u64) < size {
        body.extend_from_slice(fill.as_bytes());
    }
    body.truncate(usize::try_from(size).unwrap());
    body
}

#[test]
fn the_tip_pass_reads_its_blobs_in_batches_under_the_budget() {
    use potluck::kinds::generic::MAX_CONTENT_BYTES;
    use potluck::secret_check::scan::TIP_BLOB_BUDGET;

    const PATHS: usize = 40;
    let s = Scratch::new();
    let path = |i: usize| format!("f{i:02}.t");
    for i in 0..PATHS {
        s.write(&path(i), &big_body(b"strict", i, MAX_CONTENT_BYTES));
    }
    let old = s.commit("strict");
    s.publish("main");
    for i in 0..PATHS {
        s.write(
            &path(i),
            &big_body(b"relaxed", PATHS + i, MAX_CONTENT_BYTES),
        );
    }
    let new = s.stage_commit("relax");
    let old_blobs: Vec<String> = (0..PATHS).map(|i| s.blob(&old, &path(i))).collect();

    // Each `cat-file --batch` call is logged as one line of the names it was given.
    let log = s.dir.path().join("batches.log");
    let iso = s.isolated_git();
    let script = s.script(
        "batch-logging-git",
        &format!(
            "if [ \"$1\" = cat-file ] && [ \"$2\" = --batch ]; then\n  names=$(cat)\n  printf '%s\\n' \"$names\" | tr '\\n' ' ' >> '{log}'\n  echo >> '{log}'\n  printf '%s\\n' \"$names\" | exec '{iso}' \"$@\"\nfi\nexec '{iso}' \"$@\"\n",
            log = log.display(),
            iso = iso.display(),
        ),
    );
    let git = Git::new(script, Some(s.bare.clone()));
    let e = HookEnv::new(None, false, None).with_role(Role::Write);
    let found = scan_with(
        &e,
        Some(&rules(Act::Relax)),
        &[moved(&old, &new, "refs/heads/main")],
        &git,
    )
    .unwrap();

    let expected: Vec<String> = (0..PATHS)
        .map(|i| format!("refs/heads/main {} safety_relaxation relax_test", path(i)))
        .collect();
    assert_eq!(rows(&found), expected, "every entry judged, in entry order");
    // Only the tip pass reads an old side, so a call naming an old blob is a tip-pass batch.
    let text = std::fs::read_to_string(&log).unwrap();
    let tip_calls: Vec<usize> = text
        .lines()
        .filter(|l| l.split(' ').any(|n| old_blobs.iter().any(|b| b == n)))
        .map(|l| l.split(' ').filter(|n| !n.is_empty()).count())
        .collect();
    let most = usize::try_from(TIP_BLOB_BUDGET / MAX_CONTENT_BYTES).unwrap();
    assert!(tip_calls.len() >= 2, "batched: {tip_calls:?}");
    assert!(tip_calls.iter().all(|&n| n <= most), "{tip_calls:?}");
    assert_eq!(tip_calls.iter().sum::<usize>(), 2 * PATHS, "{tip_calls:?}");
}

#[test]
fn every_distinct_rule_check_returns_for_a_path_is_reported_in_order() {
    let s = Scratch::new();
    s.write("a.t", b"x\n");
    s.write("b.t", b"y\n");
    let tip = s.stage_commit("t");
    // The content pass walks a hash map, so repeated scans pin the order as well as the rows.
    for _ in 0..8 {
        assert_eq!(
            scan_rules(&s, &rules(Act::TwoRules), &tip).unwrap(),
            [
                "refs/heads/main a.t secret_value first",
                "refs/heads/main a.t secret_value second",
                "refs/heads/main b.t secret_value first",
                "refs/heads/main b.t secret_value second",
            ]
        );
    }
}

#[test]
fn every_rule_for_a_blob_over_the_cap_is_reported() {
    let s = Scratch::new();
    let cap = usize::try_from(potluck::kinds::generic::MAX_CONTENT_BYTES).unwrap();
    s.write("big.t", &vec![b'x'; cap + 1]);
    let tip = s.stage_commit("big");
    assert_eq!(
        scan_rules(&s, &rules(Act::TwoRules), &tip).unwrap(),
        [
            "refs/heads/main big.t secret_value first",
            "refs/heads/main big.t secret_value second",
        ]
    );
}

#[test]
fn a_rule_check_returns_twice_for_one_path_is_reported_once_with_its_first_detail() {
    let s = Scratch::new();
    s.write("a.t", b"x\n");
    let tip = s.stage_commit("t");
    let r = scan_with(
        &env(None),
        Some(&rules(Act::SameRuleTwice)),
        &[update(&tip, "refs/heads/main")],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&r), ["refs/heads/main a.t secret_value twice"]);
    assert_eq!(r[0].finding().detail(), Some("one"));
}

#[test]
fn accepted_relaxations_keeps_a_relaxation_that_follows_another_finding() {
    let s = Scratch::new();
    s.write("a.t", b"strict\n");
    let old = s.commit("strict");
    s.publish("main");
    s.write("a.t", b"relaxed\n");
    let new = s.commit("relax");
    s.publish("main");
    let got = potluck::secret_check::scan::accepted_relaxations(
        &rules(Act::PlainAndRelax),
        &[moved(&old, &new, "refs/heads/main")],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&got), RELAXED);
}

#[test]
fn every_relaxing_rule_under_every_ref_is_logged_once() {
    let s = Scratch::new();
    s.write("a.t", b"strict\n");
    let old = s.commit("strict");
    s.publish("main");
    s.write("a.t", b"relaxed\n");
    let new = s.commit("relax");
    s.publish("main");
    let updates = [
        moved(&old, &new, "refs/heads/main"),
        moved(&old, &new, "refs/heads/copy"),
    ];
    let got = potluck::secret_check::scan::accepted_relaxations(
        &rules(Act::TwoRelaxations),
        &updates,
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(
        rows(&got),
        [
            "refs/heads/main a.t safety_relaxation relax_one",
            "refs/heads/main a.t safety_relaxation relax_two",
            "refs/heads/copy a.t safety_relaxation relax_one",
            "refs/heads/copy a.t safety_relaxation relax_two",
        ]
    );
    let text = crate::logs::captured_logs(|| {
        potluck::secret_check::log_accepted_relaxations(
            7,
            "alice",
            &rules(Act::TwoRelaxations),
            &updates,
            &git_for(&s),
        );
    });
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("safety relaxation accepted"))
        .collect();
    assert_eq!(lines.len(), 4, "{text}");
    for (line, (r#ref, rule)) in lines.iter().zip([
        ("refs/heads/main", "relax_one"),
        ("refs/heads/main", "relax_two"),
        ("refs/heads/copy", "relax_one"),
        ("refs/heads/copy", "relax_two"),
    ]) {
        assert!(line.contains(r#ref) && line.contains(rule), "{line}");
    }
}
