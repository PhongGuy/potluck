//! A push that would leave the workspace's default branch missing: the hook refuses it for
//! anyone but an admin or an armed override, unless the same push creates the branch.

use std::path::PathBuf;

use potluck::role::Role;
use potluck::secret_check::git::{Git, RefUpdate};
use potluck::secret_check::scan::{HookEnv, Rejection, default_branch_missing, report, scan};

use crate::common::scratch::Scratch;

const ZERO: &str = "0000000000000000000000000000000000000000";
const MISSING: &str = "safety_relaxation default_branch_missing";

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

/// A commit staged in the bare repo on no ref; `main` does not exist there.
fn staged(s: &Scratch) -> String {
    s.write("a.txt", b"one\n");
    s.stage_commit("one")
}

#[test]
fn a_writer_creating_only_another_branch_in_an_empty_repo_is_refused() {
    let s = Scratch::new();
    let tip = staged(&s);
    let found = scan(
        &writer(),
        &[moved(ZERO, &tip, "refs/heads/feature")],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&found), [format!("refs/heads/feature / {MISSING}")]);
    let mut out = Vec::new();
    report(&found, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "potluck-reject safety_relaxation refs/heads/feature / default_branch_missing\n"
    );
}

#[test]
fn a_push_that_also_creates_the_default_branch_passes() {
    let s = Scratch::new();
    let tip = staged(&s);
    let push = [
        moved(ZERO, &tip, "refs/heads/feature"),
        moved(ZERO, &tip, "refs/heads/main"),
    ];
    assert!(rows(&scan(&writer(), &push, &git_for(&s)).unwrap()).is_empty());
}

#[test]
fn a_tags_only_push_into_an_empty_repo_is_refused() {
    let s = Scratch::new();
    let tip = staged(&s);
    assert_eq!(
        rows(
            &scan(
                &writer(),
                &[moved(ZERO, &tip, "refs/tags/v1")],
                &git_for(&s)
            )
            .unwrap()
        ),
        [format!("refs/tags/v1 / {MISSING}")]
    );
}

#[test]
fn a_repo_already_missing_the_default_branch_refuses_moves_but_not_deletes() {
    let s = Scratch::new();
    s.write("a.txt", b"one\n");
    let old = s.commit("one");
    // The server holds `feature` at `old` and no `main`; `new` is staged on no ref.
    s.branch("feature");
    s.publish("feature");
    s.write("a.txt", b"two\n");
    s.commit("two");
    let new = s.stage("feature");
    let git = git_for(&s);
    assert_eq!(
        rows(&scan(&writer(), &[moved(&old, &new, "refs/heads/feature")], &git).unwrap()),
        [format!("refs/heads/feature / {MISSING}")]
    );
    assert!(
        rows(&scan(&writer(), &[moved(&old, ZERO, "refs/heads/feature")], &git).unwrap())
            .is_empty()
    );
}

#[test]
fn a_push_beside_an_existing_default_branch_passes() {
    let s = Scratch::new();
    s.write("a.txt", b"one\n");
    let tip = s.commit("one");
    s.publish("main");
    assert!(
        rows(
            &scan(
                &writer(),
                &[moved(ZERO, &tip, "refs/heads/feature")],
                &git_for(&s)
            )
            .unwrap()
        )
        .is_empty()
    );
}

#[test]
fn an_admin_an_override_and_an_unset_default_branch_pass() {
    let s = Scratch::new();
    let tip = staged(&s);
    let push = [moved(ZERO, &tip, "refs/heads/feature")];
    let git = git_for(&s);
    let admin = writer().with_role(Role::Admin);
    let armed = HookEnv::new(None, false, Some("9")).with_default_branch("main");
    let unset = HookEnv::new(None, false, None);
    for env in [admin, armed, unset] {
        assert!(rows(&scan(&env, &push, &git).unwrap()).is_empty());
    }
}

#[test]
fn a_failing_ref_lookup_fails_closed() {
    let s = Scratch::new();
    let tip = staged(&s);
    let broken = Git::new(PathBuf::from("/nonexistent/git"), Some(s.bare.clone()));
    let push = [moved(ZERO, &tip, "refs/heads/feature")];
    // `scan` would fail earlier on its own peel, so the check is asked directly.
    assert!(default_branch_missing(Some("main"), &push, &broken).is_err());
    assert!(scan(&writer(), &push, &broken).is_err());
}
