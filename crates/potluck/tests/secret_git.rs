//! Ref lines, and the plumbing calls against a scratch repository.

use std::path::PathBuf;

use potluck::secret_check::git::{
    Git, GitCmdError, ObjectType, ParseError, RefUpdate, Sha, TipEntry,
};

use crate::common::scratch::{Scratch, staged_history};

const ZERO: &str = "0000000000000000000000000000000000000000";

fn sha(s: &str) -> Sha {
    Sha::parse(s).unwrap()
}

#[test]
fn ref_update_lines_parse_and_a_zero_new_is_a_delete() {
    let a = "a".repeat(40);
    let u = RefUpdate::parse_line(format!("{ZERO} {a} refs/heads/main\n").as_bytes()).unwrap();
    assert!(u.old().is_zero());
    assert!(!u.is_delete());
    assert_eq!(u.new().as_str(), a);
    assert_eq!(u.name(), b"refs/heads/main");
    assert!(
        RefUpdate::parse_line(format!("{a} {ZERO} refs/heads/gone").as_bytes())
            .unwrap()
            .is_delete()
    );
    assert!(matches!(
        RefUpdate::parse_line(b"x y").unwrap_err(),
        ParseError::Shape { .. }
    ));
    assert!(matches!(
        RefUpdate::parse_line(format!("{ZERO} {a} refs/heads/two words").as_bytes()).unwrap_err(),
        ParseError::Shape { .. }
    ));
    assert!(matches!(
        RefUpdate::parse_line(format!("{ZERO} {a} ").as_bytes()).unwrap_err(),
        ParseError::Shape { .. }
    ));
    assert!(matches!(
        RefUpdate::parse_line(format!("{ZERO} nothex refs/heads/x").as_bytes()).unwrap_err(),
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
fn a_ref_name_keeps_its_exact_bytes() {
    let a = "a".repeat(40);
    let mut line = format!("{ZERO} {a} ").into_bytes();
    line.extend_from_slice(b"refs/heads/caf\xe9\n");
    let u = RefUpdate::parse_line(&line).unwrap();
    assert_eq!(u.name(), b"refs/heads/caf\xe9");
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

#[test]
fn tip_diff_lists_added_changed_and_deleted_paths_with_both_blobs() {
    let s = Scratch::new();
    s.write("keep.txt", b"k\n");
    s.write("change.t", b"strict\n");
    s.write("gone.t", b"g\n");
    let old = s.commit("old");
    s.remove("gone.t");
    s.write("change.t", b"relaxed\n");
    s.write("new dir/added.t", b"a\n");
    let new = s.commit("new");
    s.publish("main");
    let mut got = git_for(&s).tip_diff(&sha(&old), &sha(&new)).unwrap();
    got.sort_by(|a, b| a.path().cmp(b.path()));
    let view: Vec<(String, Option<String>, Option<String>)> = got
        .iter()
        .map(|e: &TipEntry| {
            (
                e.path_lossy(),
                e.old().map(|x| x.as_str().to_owned()),
                e.new().map(|x| x.as_str().to_owned()),
            )
        })
        .collect();
    assert_eq!(
        view,
        [
            (
                "change.t".to_owned(),
                Some(s.blob(&old, "change.t")),
                Some(s.blob(&new, "change.t"))
            ),
            ("gone.t".to_owned(), Some(s.blob(&old, "gone.t")), None),
            (
                "new dir/added.t".to_owned(),
                None,
                Some(s.blob(&new, "new dir/added.t"))
            ),
        ]
    );
    assert!(
        git_for(&s)
            .tip_diff(&sha(&new), &sha(&new))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn blob_at_finds_a_root_file_and_nothing_else() {
    let s = Scratch::new();
    s.write("opencollection.yml", b"opencollection: 1.0.0\n");
    s.write("dir.yml/inner", b"x\n");
    let tip = s.commit("c");
    s.publish("main");
    let g = git_for(&s);
    assert_eq!(
        g.blob_at(&sha(&tip), "opencollection.yml")
            .unwrap()
            .map(|x| x.as_str().to_owned()),
        Some(s.blob(&tip, "opencollection.yml"))
    );
    assert_eq!(g.blob_at(&sha(&tip), "missing.yml").unwrap(), None);
    assert_eq!(
        g.blob_at(&sha(&tip), "missing dir/x.yml").unwrap(),
        None,
        "git's missing line for a path with a space"
    );
    assert_eq!(
        g.blob_at(&sha(&tip), "dir.yml").unwrap(),
        None,
        "a tree is not a blob"
    );
}

#[test]
fn tip_diff_reads_a_gitlink_side_as_absent() {
    let s = Scratch::new();
    s.write("README", b"r\n");
    let seed = s.commit("seed");
    s.write("sub", b"was a file\n");
    let old = s.commit("old");
    s.remove("sub");
    s.git(Some(&s.work), &["rm", "-q", "--cached", "sub"]);
    s.git(
        Some(&s.work),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{seed},sub"),
        ],
    );
    // Commit the index as it stands: `Scratch::commit` runs `add -A`, which drops a gitlink
    // that has no work-tree directory.
    s.git(Some(&s.work), &["commit", "-q", "-m", "new"]);
    let new = s.work_git(&["rev-parse", "HEAD"]);
    assert_eq!(
        s.work_git(&["ls-tree", &new, "sub"]),
        format!("160000 commit {seed}\tsub")
    );
    s.publish("main");
    let got = git_for(&s).tip_diff(&sha(&old), &sha(&new)).unwrap();
    let view: Vec<(String, Option<String>, Option<String>)> = got
        .iter()
        .map(|e| {
            (
                e.path_lossy(),
                e.old().map(|x| x.as_str().to_owned()),
                e.new().map(|x| x.as_str().to_owned()),
            )
        })
        .collect();
    assert_eq!(
        view,
        [("sub".to_owned(), Some(s.blob(&old, "sub")), None)],
        "a blob replaced by a gitlink has no blob on the new side"
    );
}
