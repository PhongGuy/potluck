//! Ref lines, and the plumbing calls against a scratch repository.

use std::path::PathBuf;

use potluck::secret_check::git::{Git, GitCmdError, ObjectType, ParseError, RefUpdate, Sha};

use crate::common::scratch::{Scratch, staged_history};

const ZERO: &str = "0000000000000000000000000000000000000000";

fn sha(s: &str) -> Sha {
    Sha::parse(s).unwrap()
}

#[test]
fn ref_update_lines_parse_and_a_zero_new_is_a_delete() {
    let a = "a".repeat(40);
    let u = RefUpdate::parse_line(&format!("{ZERO} {a} refs/heads/main\n")).unwrap();
    assert!(u.old().is_zero());
    assert!(!u.is_delete());
    assert_eq!(u.new().as_str(), a);
    assert_eq!(u.name(), "refs/heads/main");
    assert!(
        RefUpdate::parse_line(&format!("{a} {ZERO} refs/heads/gone"))
            .unwrap()
            .is_delete()
    );
    assert!(matches!(
        RefUpdate::parse_line("x y").unwrap_err(),
        ParseError::Shape { .. }
    ));
    assert!(matches!(
        RefUpdate::parse_line(&format!("{ZERO} {a} refs/heads/two words")).unwrap_err(),
        ParseError::Shape { .. }
    ));
    assert!(matches!(
        RefUpdate::parse_line(&format!("{ZERO} {a} ")).unwrap_err(),
        ParseError::Shape { .. }
    ));
    assert!(matches!(
        RefUpdate::parse_line(&format!("{ZERO} nothex refs/heads/x")).unwrap_err(),
        ParseError::NotHex { .. }
    ));
    assert_eq!(
        Sha::parse(&"F".repeat(64)).unwrap().as_str(),
        "f".repeat(64),
        "SHA-256 width, folded to lower case"
    );
    assert!(Sha::parse("abcdef").is_err(), "wrong width");
    assert_eq!(sha(ZERO).to_string(), ZERO);
}

#[test]
fn scratch_stages_objects_without_refs_and_publishes_with_them() {
    let s = Scratch::new();
    s.write("README", b"seed\n");
    let tip = s.commit("seed");
    assert_eq!(s.stage("main"), tip);
    assert_eq!(
        s.bare_git(&["for-each-ref"]),
        "",
        "staged: no ref points at the objects"
    );
    assert_eq!(
        s.bare_git(&["cat-file", "-t", &tip]),
        "commit",
        "staged: the objects are there"
    );
    s.publish("main");
    assert_eq!(s.bare_git(&["rev-parse", "refs/heads/main"]), tip);
}

fn git_for(s: &Scratch) -> Git {
    Git::new(s.isolated_git(), Some(s.bare.clone()))
}

#[test]
fn new_commits_exclude_what_the_server_has_and_what_the_caller_names() {
    let s = Scratch::new();
    let h = staged_history(&s);
    let git = git_for(&s);
    let all = git.new_commits(&sha(&h.tip), &[]).unwrap();
    assert_eq!(all.len(), 5, "env, more, merge, add, remove: {all:?}");
    assert!(all.contains(&sha(&h.merge)));
    assert!(
        !all.contains(&sha(&h.root)),
        "the published seed is not new"
    );
    assert_eq!(
        git.new_commits(&sha(&h.tip), &[sha(&h.merge)])
            .unwrap()
            .len(),
        2,
        "only add and remove sit above the merge"
    );
}

#[test]
fn new_objects_are_the_blobs_the_server_lacks() {
    let s = Scratch::new();
    let h = staged_history(&s);
    let objects = git_for(&s).new_objects(&sha(&h.tip), &[]).unwrap();
    assert!(objects.contains(&sha(&h.env_blob)));
    assert!(objects.contains(&sha(&h.creds_blob)));
    assert!(objects.contains(&sha(&h.merge)), "commits are objects too");
    assert!(
        !objects.contains(&sha(&h.seed_readme)),
        "the seed README blob is already there"
    );
}

#[test]
fn peel_reports_the_target_type() {
    let s = Scratch::new();
    let h = staged_history(&s);
    let git = git_for(&s);
    assert_eq!(
        git.peel(&sha(&h.tip)).unwrap(),
        (sha(&h.tip), ObjectType::Commit)
    );
    let tree = s.work_git(&["rev-parse", "main^{tree}"]);
    s.tag("tree-tag", &tree);
    s.tag("blob-tag", &h.creds_blob);
    s.tag("tag-tag", "refs/tags/blob-tag");
    let tree_tag = s.stage("refs/tags/tree-tag");
    let blob_tag = s.stage("refs/tags/blob-tag");
    let tag_tag = s.stage("refs/tags/tag-tag");
    assert_eq!(
        git.peel(&sha(&tree_tag)).unwrap(),
        (sha(&tree), ObjectType::Tree)
    );
    assert_eq!(
        git.peel(&sha(&blob_tag)).unwrap(),
        (sha(&h.creds_blob), ObjectType::Blob)
    );
    assert_eq!(
        git.peel(&sha(&tag_tag)).unwrap(),
        (sha(&h.creds_blob), ObjectType::Blob),
        "a tag of a tag peels through"
    );
    assert!(
        matches!(
            git.peel(&sha(ZERO)).unwrap_err(),
            GitCmdError::Output {
                subcommand: "cat-file",
                ..
            }
        ),
        "a missing object"
    );
    assert!(
        git.new_commits(&sha(&blob_tag), &[]).unwrap().is_empty(),
        "rev-list sees no commit behind a blob tag"
    );
    let no_binary = Git::new(PathBuf::from("/nonexistent/git"), Some(s.bare.clone()));
    assert!(
        matches!(
            no_binary.peel(&sha(ZERO)).unwrap_err(),
            GitCmdError::Spawn { .. }
        ),
        "a missing binary is Spawn, not a panic"
    );
}

#[test]
fn a_failing_git_flooding_both_pipes_keeps_4_kib_of_stderr_and_returns() {
    let s = Scratch::new();
    // 3 MB on stderr before 3 MB on stdout: a runner that does not drain stderr while
    // reading stdout blocks here, so the bounded wait below also pins the drain.
    let fake = s.script(
        "flooding-git",
        "head -c 3000000 /dev/zero | tr '\\0' e >&2\nhead -c 3000000 /dev/zero | tr '\\0' o\nexit 3\n",
    );
    let git = Git::new(fake, Some(s.bare.clone()));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(git.new_commits(&sha(&"a".repeat(40)), &[]));
    });
    let result = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the call returns");
    match result {
        Err(GitCmdError::Failed { stderr, .. }) => {
            assert_eq!(stderr.len(), 4096, "stderr is kept up to 4 KiB");
            assert!(
                stderr.bytes().all(|b| b == b'e'),
                "only stderr bytes are kept"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn new_tags_list_the_pushed_tag_chain_and_skip_tags_the_server_has() {
    let s = Scratch::new();
    let h = staged_history(&s);
    let git = git_for(&s);
    assert!(
        git.new_tags(&sha(&h.tip), &[]).unwrap().is_empty(),
        "a commit tip is no tag"
    );
    s.tag("inner", &h.root);
    s.tag("outer", "refs/tags/inner");
    s.tag("blob-tag", &h.creds_blob);
    let inner = s.stage("refs/tags/inner");
    let outer = s.stage("refs/tags/outer");
    let blob_tag = s.stage("refs/tags/blob-tag");
    let sorted = |mut v: Vec<Sha>| {
        v.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        v
    };
    let chain = sorted(vec![sha(&outer), sha(&inner)]);
    assert_eq!(
        sorted(git.new_tags(&sha(&outer), &[]).unwrap()),
        chain,
        "both tags of the chain, the published root commit left out"
    );
    assert_eq!(
        sorted(git.new_tags(&sha(&outer), &[sha(&h.tip)]).unwrap()),
        chain,
        "an excluded commit tip hides no tag"
    );
    assert_eq!(
        git.new_tags(&sha(&blob_tag), &[]).unwrap(),
        [sha(&blob_tag)],
        "a tag of a blob is listed too"
    );
    // The inner tag lands under a ref, as an accepted earlier push would have left it.
    let work = s.work.to_str().unwrap().to_owned();
    s.bare_git(&[
        "fetch",
        "-q",
        "--no-tags",
        &work,
        "+refs/tags/inner:refs/tags/inner",
    ]);
    assert_eq!(git.new_tags(&sha(&outer), &[]).unwrap(), [sha(&outer)]);
}
