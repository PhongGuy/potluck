//! Changed paths, whole trees and blob contents from the scratch repository.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use potluck::secret_check::git::{Entry, Git, GitCmdError, Sha};

use crate::common::scratch::{Scratch, staged_history};

const ZERO: &str = "0000000000000000000000000000000000000000";

fn git_for(s: &Scratch) -> Git {
    Git::new(s.isolated_git(), Some(s.bare.clone()))
}

fn sha(s: &str) -> Sha {
    Sha::parse(s).unwrap()
}

fn paths(entries: &[Entry]) -> Vec<String> {
    let mut v: Vec<String> = entries.iter().map(Entry::path_lossy).collect();
    v.sort_unstable();
    v
}

fn raw_paths(entries: &[Entry]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = entries.iter().map(Entry::path).collect();
    v.sort_unstable();
    v
}

#[test]
fn changed_entries_cover_merges_roots_and_spaces_and_skip_deletions() {
    let s = Scratch::new();
    let h = staged_history(&s);
    let git = git_for(&s);
    let merge = git.changed_entries(&[sha(&h.merge)]).unwrap();
    assert_eq!(
        paths(&merge),
        ["README", "environments/dev.yml", "notes/with space.txt"],
        "each parent's side of the merge"
    );
    assert_eq!(
        paths(&git.changed_entries(&[sha(&h.root)]).unwrap()),
        ["README"],
        "a root commit's whole tree"
    );
    assert!(
        git.changed_entries(&[sha(&h.tip)]).unwrap().is_empty(),
        "the removal commit adds nothing"
    );
    assert!(git.changed_entries(&[]).unwrap().is_empty());
    let all = git
        .changed_entries(&git.new_commits(&sha(&h.tip), &[]).unwrap())
        .unwrap();
    let creds: Vec<&Entry> = all.iter().filter(|e| e.path() == b"creds.txt").collect();
    assert_eq!(creds.len(), 1, "added once, removed once: listed once");
    assert_eq!(creds[0].sha(), &sha(&h.creds_blob));
    assert_eq!(
        all.iter()
            .filter(|e| e.path() == b"environments/dev.yml")
            .count(),
        1,
        "deduplicated across the feat commit and the merge"
    );
}

#[test]
fn the_same_blob_at_two_paths_is_listed_twice() {
    let s = Scratch::new();
    s.write("notes/dev.yml", b"same\n");
    s.write("environments/dev.yml", b"same\n");
    let tip = s.commit("twins");
    s.stage("main");
    let entries = git_for(&s).changed_entries(&[sha(&tip)]).unwrap();
    assert_eq!(paths(&entries), ["environments/dev.yml", "notes/dev.yml"]);
    assert_eq!(entries[0].sha(), entries[1].sha());
}

#[test]
fn a_non_utf8_path_keeps_its_bytes() {
    let s = Scratch::new();
    let dir = s.work.join(OsStr::from_bytes(b"caf\xe9"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".env"), b"x\n").unwrap();
    std::fs::write(s.work.join(OsStr::from_bytes(b"caf\xe9.txt")), b"t\n").unwrap();
    let tip = s.commit("latin-1 names");
    s.stage("main");
    let git = git_for(&s);
    let expected: [&[u8]; 2] = [b"caf\xe9.txt", b"caf\xe9/.env"];
    assert_eq!(
        raw_paths(&git.changed_entries(&[sha(&tip)]).unwrap()),
        expected
    );
    assert_eq!(raw_paths(&git.tree_entries(&sha(&tip)).unwrap()), expected);
    assert_eq!(
        paths(&git.tree_entries(&sha(&tip)).unwrap()),
        ["caf\u{fffd}.txt", "caf\u{fffd}/.env"],
        "lossy only for display"
    );
}

#[test]
fn tree_entries_list_a_commit_a_tree_or_a_tag_of_one() {
    let s = Scratch::new();
    let h = staged_history(&s);
    let git = git_for(&s);
    let expected = ["README", "environments/dev.yml", "notes/with space.txt"];
    assert_eq!(paths(&git.tree_entries(&sha(&h.tip)).unwrap()), expected);
    let tree = s.work_git(&["rev-parse", "main^{tree}"]);
    assert_eq!(paths(&git.tree_entries(&sha(&tree)).unwrap()), expected);
    s.tag("tree-tag", &tree);
    let tag = s.stage("refs/tags/tree-tag");
    assert_eq!(paths(&git.tree_entries(&sha(&tag)).unwrap()), expected);
    assert!(
        matches!(
            git.tree_entries(&sha(&h.creds_blob)).unwrap_err(),
            GitCmdError::Failed {
                subcommand: "ls-tree",
                ..
            }
        ),
        "a blob has no tree"
    );
    let bad_dir = Git::new(s.isolated_git(), Some(s.dir.path().join("nope.git")));
    assert!(
        matches!(
            bad_dir.tree_entries(&sha(&h.tip)).unwrap_err(),
            GitCmdError::Failed {
                subcommand: "ls-tree",
                ..
            }
        ),
        "a missing repository is Failed, not a panic"
    );
}

#[test]
fn blob_sizes_skip_missing_names_and_non_blobs() {
    let s = Scratch::new();
    let h = staged_history(&s);
    let sizes = git_for(&s)
        .blob_sizes(&[sha(&h.creds_blob), sha(ZERO), sha(&h.merge)])
        .unwrap();
    assert_eq!(sizes.get(&sha(&h.creds_blob)), Some(&21));
    assert!(!sizes.contains_key(&sha(ZERO)), "missing");
    assert!(
        !sizes.contains_key(&sha(&h.merge)),
        "a commit is not a blob"
    );
    assert!(git_for(&s).blob_sizes(&[]).unwrap().is_empty());
}

#[test]
fn six_thousand_names_do_not_deadlock_batch_check() {
    // 6000 names in (246 KiB) and 6000 `missing` lines out (281 KiB) both exceed the pipe buffer.
    let s = Scratch::new();
    staged_history(&s);
    let names: Vec<Sha> = (0..6000u32).map(|i| sha(&format!("{i:040x}"))).collect();
    assert!(git_for(&s).blob_sizes(&names).unwrap().is_empty());
}

#[test]
fn blobs_stream_in_order_and_skip_missing_names_and_non_blobs() {
    let s = Scratch::new();
    let h = staged_history(&s);
    let mut seen = Vec::new();
    git_for(&s)
        .for_each_blob(
            &[
                sha(&h.creds_blob),
                sha(ZERO),
                sha(&h.merge),
                sha(&h.env_blob),
            ],
            |sha, bytes| seen.push((sha.clone(), bytes.to_vec())),
        )
        .unwrap();
    assert_eq!(
        seen,
        [
            (sha(&h.creds_blob), b"AKIAIOSFODNN7EXAMPLE\n".to_vec()),
            (sha(&h.env_blob), b"name: dev\n".to_vec())
        ]
    );
    let mut none = 0;
    git_for(&s).for_each_blob(&[], |_, _| none += 1).unwrap();
    assert_eq!(none, 0);
}

#[test]
fn six_thousand_commits_do_not_deadlock_diff_tree() {
    // 6000 names in (246 KiB) and 6000 copies of the merge's diff out exceed the pipe buffer.
    let s = Scratch::new();
    let h = staged_history(&s);
    let git = git_for(&s);
    let many = vec![sha(&h.merge); 6000];
    assert_eq!(
        git.changed_entries(&many).unwrap(),
        git.changed_entries(&[sha(&h.merge)]).unwrap(),
        "deduplicated to one copy"
    );
}
