//! Ref namespaces: a non-admin may write only branches and tags.

use std::path::PathBuf;

use potluck::role::Role;
use potluck::secret_check::git::{Git, RefUpdate};
use potluck::secret_check::scan::{HookEnv, Rejection, foreign_refs, scan};

use crate::common::scratch::Scratch;

const ZERO: &str = "0000000000000000000000000000000000000000";

fn git_for(s: &Scratch) -> Git {
    Git::new(s.isolated_git(), Some(s.bare.clone()))
}

fn moved(old: &str, new: &str, name: &str) -> RefUpdate {
    RefUpdate::parse_line(format!("{old} {new} {name}").as_bytes()).unwrap()
}

/// A write-role pusher on a workspace whose default branch is `main`.
fn writer() -> HookEnv {
    HookEnv::new(None, false, None).with_default_branch("main")
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

fn line(name: &str) -> String {
    format!("{name} / safety_relaxation ref_namespace")
}

/// A commit staged in the bare repository on no ref, ready to be pushed anywhere.
fn staged(s: &Scratch) -> String {
    s.write("a.txt", b"one\n");
    s.stage_commit("one")
}

#[test]
fn a_writer_creating_a_notes_ref_is_refused_with_one_ref_namespace_line() {
    let s = Scratch::new();
    let tip = staged(&s);
    let found = scan(
        &writer(),
        &[moved(ZERO, &tip, "refs/notes/x")],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&found), [line("refs/notes/x")]);
}

#[test]
fn a_writer_delete_outside_heads_and_tags_is_refused_without_any_git_call() {
    let s = Scratch::new();
    let tip = staged(&s);
    let broken = Git::new(PathBuf::from("/nonexistent/git"), Some(s.bare.clone()));
    let delete = || moved(&tip, ZERO, "refs/foo/x");
    let found = scan(&writer(), &[delete()], &broken).unwrap();
    assert_eq!(rows(&found), [line("refs/foo/x")]);
    // An unknown kind skips the kind lookup for an all-delete push, but not this rule.
    let unknown = HookEnv::new(Some("no-such-kind"), false, None).with_default_branch("main");
    let found = scan(&unknown, &[delete()], &broken).unwrap();
    assert_eq!(rows(&found), [line("refs/foo/x")]);
}

#[test]
fn a_prefix_without_its_trailing_slash_is_refused() {
    let s = Scratch::new();
    let tip = staged(&s);
    let git = git_for(&s);
    for name in ["refs/headsx/y", "refs/tagsx/y"] {
        let found = scan(&writer(), &[moved(ZERO, &tip, name)], &git).unwrap();
        assert_eq!(rows(&found), [line(name)], "{name}");
    }
}

#[test]
fn a_writer_may_create_a_branch_and_a_tag() {
    let s = Scratch::new();
    let tip = staged(&s);
    let git = git_for(&s);
    for name in ["refs/heads/t", "refs/tags/v1"] {
        let found = scan(&writer(), &[moved(ZERO, &tip, name)], &git).unwrap();
        assert!(found.is_empty(), "{name}: {:?}", rows(&found));
    }
}

#[test]
fn an_admin_or_an_armed_override_may_write_any_ref() {
    let s = Scratch::new();
    let tip = staged(&s);
    let git = git_for(&s);
    let admin = writer().with_role(Role::Admin);
    let armed = HookEnv::new(None, false, Some("9")).with_default_branch("main");
    for env in [admin, armed] {
        let found = scan(&env, &[moved(ZERO, &tip, "refs/notes/x")], &git).unwrap();
        assert!(found.is_empty(), "{env:?}: {:?}", rows(&found));
        let found = scan(&env, &[moved(&tip, ZERO, "refs/foo/x")], &git).unwrap();
        assert!(found.is_empty(), "{env:?}: {:?}", rows(&found));
    }
}

#[test]
fn only_the_foreign_ref_of_a_mixed_push_is_refused() {
    let s = Scratch::new();
    let tip = staged(&s);
    let found = scan(
        &writer(),
        &[
            moved(ZERO, &tip, "refs/heads/t"),
            moved(ZERO, &tip, "refs/remotes/origin/t"),
        ],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&found), [line("refs/remotes/origin/t")]);
}

#[test]
fn foreign_refs_lists_each_foreign_update_in_order() {
    let sha = "1111111111111111111111111111111111111111";
    let updates = [
        moved(ZERO, sha, "refs/remotes/origin/main"),
        moved(ZERO, sha, "refs/heads/main"),
        moved(sha, ZERO, "refs/notes/commits"),
        moved(ZERO, sha, "refs/tags/v1"),
        moved(sha, sha, "refs/pull/1/head"),
    ];
    assert_eq!(
        rows(&foreign_refs(&updates)),
        [
            line("refs/remotes/origin/main"),
            line("refs/notes/commits"),
            line("refs/pull/1/head"),
        ]
    );
}
