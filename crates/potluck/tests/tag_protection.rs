//! Tag protection: a non-admin may create a tag but not move or delete one.

use potluck::role::Role;
use potluck::secret_check::git::{Git, RefUpdate};
use potluck::secret_check::scan::{HookEnv, Rejection, scan, tag_rewrites};

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

fn line(name: &str, rule: &str) -> String {
    format!("{name} / safety_relaxation {rule}")
}

/// Two commits on the server, `main` at the first and the second staged with no ref, so a
/// tag can be pointed at either; returns (first, second).
fn two_commits(s: &Scratch) -> (String, String) {
    s.write("a.txt", b"one\n");
    let first = s.commit("one");
    s.publish("main");
    s.write("a.txt", b"two\n");
    let second = s.commit("two");
    s.stage("main");
    (first, second)
}

#[test]
fn a_writer_moving_a_tag_is_refused_with_tag_move() {
    let s = Scratch::new();
    let (first, second) = two_commits(&s);
    let found = scan(
        &writer(),
        &[moved(&first, &second, "refs/tags/v1")],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&found), [line("refs/tags/v1", "tag_move")]);
}

#[test]
fn a_writer_deleting_a_tag_is_refused_with_tag_delete() {
    let s = Scratch::new();
    let (first, _) = two_commits(&s);
    let found = scan(
        &writer(),
        &[moved(&first, ZERO, "refs/tags/v1")],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&found), [line("refs/tags/v1", "tag_delete")]);
}

#[test]
fn only_the_tag_delete_of_a_push_mixed_with_a_branch_is_refused() {
    let s = Scratch::new();
    let (first, second) = two_commits(&s);
    let found = scan(
        &writer(),
        &[
            moved(ZERO, &second, "refs/heads/t"),
            moved(&first, ZERO, "refs/tags/v1"),
        ],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&found), [line("refs/tags/v1", "tag_delete")]);
}

#[test]
fn an_annotated_tag_replaced_on_the_same_commit_is_a_tag_move() {
    let s = Scratch::new();
    let (first, _) = two_commits(&s);
    let old = s.tag("v1", &first);
    s.stage("refs/tags/v1");
    s.work_git(&["tag", "-d", "v1"]);
    let new = s.tag_message("v1", &first, "again");
    s.stage("refs/tags/v1");
    assert_ne!(old, new);
    let found = scan(
        &writer(),
        &[moved(&old, &new, "refs/tags/v1")],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&found), [line("refs/tags/v1", "tag_move")]);
}

#[test]
fn a_writer_may_create_a_lightweight_or_annotated_tag_and_repeat_one() {
    let s = Scratch::new();
    let (first, _) = two_commits(&s);
    let annotated = s.tag("v2", &first);
    s.stage("refs/tags/v2");
    let git = git_for(&s);
    for update in [
        moved(ZERO, &first, "refs/tags/v1"),
        moved(ZERO, &annotated, "refs/tags/v2"),
        moved(&first, &first, "refs/tags/v1"),
    ] {
        let found = scan(&writer(), std::slice::from_ref(&update), &git).unwrap();
        assert!(found.is_empty(), "{update:?}: {:?}", rows(&found));
    }
}

#[test]
fn an_admin_or_an_armed_override_may_move_and_delete_a_tag() {
    let s = Scratch::new();
    let (first, second) = two_commits(&s);
    let git = git_for(&s);
    let admin = writer().with_role(Role::Admin);
    let armed = HookEnv::new(None, false, Some("9")).with_default_branch("main");
    for env in [admin, armed] {
        let found = scan(&env, &[moved(&first, &second, "refs/tags/v1")], &git).unwrap();
        assert!(found.is_empty(), "{env:?}: {:?}", rows(&found));
        let found = scan(&env, &[moved(&first, ZERO, "refs/tags/v1")], &git).unwrap();
        assert!(found.is_empty(), "{env:?}: {:?}", rows(&found));
    }
}

#[test]
fn tag_refusals_follow_update_order() {
    let s = Scratch::new();
    let (first, second) = two_commits(&s);
    let found = scan(
        &writer(),
        &[
            moved(&first, &second, "refs/tags/v2"),
            moved(ZERO, &second, "refs/tags/v3"),
            moved(&first, ZERO, "refs/tags/v1"),
        ],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(
        rows(&found),
        [
            line("refs/tags/v2", "tag_move"),
            line("refs/tags/v1", "tag_delete"),
        ]
    );
}

#[test]
fn tag_rewrites_lists_each_tag_move_and_delete_in_order() {
    let a = "1111111111111111111111111111111111111111";
    let b = "2222222222222222222222222222222222222222";
    let updates = [
        moved(ZERO, a, "refs/tags/new"),
        moved(a, b, "refs/tags/moved"),
        moved(a, b, "refs/heads/main"),
        moved(a, a, "refs/tags/same"),
        moved(a, ZERO, "refs/tags/gone"),
        moved(a, b, "refs/tagsx/y"),
    ];
    assert_eq!(
        rows(&tag_rewrites(&updates)),
        [
            line("refs/tags/moved", "tag_move"),
            line("refs/tags/gone", "tag_delete"),
        ]
    );
}

#[test]
fn an_accepted_tag_rewrite_is_logged_with_workspace_pusher_override_and_rule() {
    let a = "1111111111111111111111111111111111111111";
    let b = "2222222222222222222222222222222222222222";
    let log = |update: RefUpdate| {
        crate::logs::captured_logs(|| {
            potluck::secret_check::log_tag_rewrites(7, "alice", Some(3), &[update]);
        })
    };
    for (update, rule) in [
        (moved(a, b, "refs/tags/v1"), "tag_move"),
        (moved(a, ZERO, "refs/tags/v1"), "tag_delete"),
    ] {
        let text = log(update);
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("tag rewritten"))
            .collect();
        assert_eq!(lines.len(), 1, "{text}");
        for needle in [
            " INFO ",
            "workspace_id=7",
            "alice",
            "override_id=3",
            "refs/tags/v1",
            rule,
        ] {
            assert!(lines[0].contains(needle), "{needle} in {}", lines[0]);
        }
    }
    for update in [moved(ZERO, a, "refs/tags/v1"), moved(a, b, "refs/heads/t")] {
        let text = log(update);
        assert!(!text.contains("tag rewritten"), "{text}");
    }
}
