//! Default-branch protection: only an admin or an armed override may force-push or delete it.

use std::path::PathBuf;

use potluck::role::Role;
use potluck::secret_check::git::{Git, GitCmdError, RefUpdate};
use potluck::secret_check::scan::{HookEnv, Rejection, ScanError, report, scan};

use crate::common::scratch::Scratch;

const ZERO: &str = "0000000000000000000000000000000000000000";
const FORCE: &[&str] = &["refs/heads/main / safety_relaxation force_push"];
const DELETE: &[&str] = &["refs/heads/main / safety_relaxation branch_delete"];

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

fn scan_rows(env: &HookEnv, u: RefUpdate, git: &Git) -> Vec<String> {
    rows(&scan(env, &[u], git).unwrap())
}

/// `main` published at `old`; `new` is a sibling of `old` on the same base, staged on no ref.
fn rewritten(s: &Scratch) -> (String, String) {
    s.write("a.txt", b"one\n");
    let base = s.commit("base");
    s.write("a.txt", b"two\n");
    let old = s.commit("old");
    s.publish("main");
    s.git(Some(&s.work), &["reset", "-q", "--hard", &base]);
    s.write("a.txt", b"three\n");
    let new = s.stage_commit("rewrite");
    (old, new)
}

/// A scratch pair like `Scratch::new`, in SHA-256.
fn sha256_scratch() -> Scratch {
    let dir = tempfile::tempdir().unwrap();
    let bare = dir.path().join("bare.git");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let s = Scratch { dir, bare, work };
    let bare_path = s.bare.to_str().unwrap().to_owned();
    s.git(
        None,
        &[
            "init",
            "-q",
            "--bare",
            "--object-format=sha256",
            "-b",
            "main",
            &bare_path,
        ],
    );
    s.git(
        Some(&s.work),
        &["init", "-q", "--object-format=sha256", "-b", "main"],
    );
    s
}

#[test]
fn a_writer_force_push_to_the_default_branch_is_refused_with_one_force_push_line() {
    let s = Scratch::new();
    let (old, new) = rewritten(&s);
    let found = scan(
        &writer(),
        &[moved(&old, &new, "refs/heads/main")],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&found), FORCE);
    let mut out = Vec::new();
    report(&found, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "potluck-reject safety_relaxation refs/heads/main / force_push\n"
    );
}

#[test]
fn a_writer_delete_of_the_default_branch_is_refused_without_any_git_call() {
    let s = Scratch::new();
    s.write("a.txt", b"one\n");
    let tip = s.commit("one");
    s.publish("main");
    let broken = Git::new(PathBuf::from("/nonexistent/git"), Some(s.bare.clone()));
    let found = scan(&writer(), &[moved(&tip, ZERO, "refs/heads/main")], &broken).unwrap();
    assert_eq!(rows(&found), DELETE);
    let mut out = Vec::new();
    report(&found, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "potluck-reject safety_relaxation refs/heads/main / branch_delete\n"
    );
    // An unknown kind skips the kind lookup for an all-delete push, but not this rule.
    let unknown = HookEnv::new(Some("no-such-kind"), false, None).with_default_branch("main");
    assert_eq!(
        scan_rows(&unknown, moved(&tip, ZERO, "refs/heads/main"), &broken),
        DELETE
    );
}

#[test]
fn an_admin_may_force_push_and_delete_the_default_branch() {
    let s = Scratch::new();
    let (old, new) = rewritten(&s);
    let admin = writer().with_role(Role::Admin);
    let git = git_for(&s);
    assert!(scan_rows(&admin, moved(&old, &new, "refs/heads/main"), &git).is_empty());
    assert!(scan_rows(&admin, moved(&old, ZERO, "refs/heads/main"), &git).is_empty());
}

#[test]
fn an_armed_override_passes_a_writer_force_push_and_delete() {
    let s = Scratch::new();
    let (old, new) = rewritten(&s);
    let armed = HookEnv::new(None, false, Some("9")).with_default_branch("main");
    let git = git_for(&s);
    assert!(scan_rows(&armed, moved(&old, &new, "refs/heads/main"), &git).is_empty());
    assert!(scan_rows(&armed, moved(&old, ZERO, "refs/heads/main"), &git).is_empty());
}

#[test]
fn a_fast_forward_and_a_no_op_of_the_default_branch_pass() {
    let s = Scratch::new();
    s.write("a.txt", b"one\n");
    let old = s.commit("one");
    s.publish("main");
    s.write("a.txt", b"two\n");
    let new = s.stage_commit("two");
    let git = git_for(&s);
    assert!(scan_rows(&writer(), moved(&old, &new, "refs/heads/main"), &git).is_empty());
    assert!(scan_rows(&writer(), moved(&old, &old, "refs/heads/main"), &git).is_empty());
}

#[test]
fn a_force_push_or_delete_of_another_ref_passes() {
    let s = Scratch::new();
    let (old, new) = rewritten(&s);
    let git = git_for(&s);
    for name in ["refs/heads/feat", "refs/heads/mainline", "refs/tags/main"] {
        assert!(
            scan_rows(&writer(), moved(&old, &new, name), &git).is_empty(),
            "{name}"
        );
        assert!(
            scan_rows(&writer(), moved(&old, ZERO, name), &git).is_empty(),
            "{name}"
        );
    }
}

#[test]
fn an_unset_or_empty_default_branch_protects_every_branch_but_no_tag() {
    let s = Scratch::new();
    let (old, new) = rewritten(&s);
    let git = git_for(&s);
    for unset in [
        HookEnv::new(None, false, None),
        HookEnv::new(None, false, None).with_default_branch(""),
    ] {
        assert_eq!(
            scan_rows(&unset, moved(&old, &new, "refs/heads/feat"), &git),
            ["refs/heads/feat / safety_relaxation force_push"]
        );
        assert_eq!(
            scan_rows(&unset, moved(&old, ZERO, "refs/heads/feat"), &git),
            ["refs/heads/feat / safety_relaxation branch_delete"]
        );
        assert!(scan_rows(&unset, moved(&old, ZERO, "refs/tags/v1"), &git).is_empty());
    }
}

#[test]
fn a_default_branch_moved_to_a_tree_counts_as_a_rewrite() {
    let s = Scratch::new();
    s.write("a.txt", b"one\n");
    let old = s.commit("one");
    s.publish("main");
    let tree = s.work_git(&["rev-parse", "HEAD^{tree}"]);
    assert_eq!(
        scan_rows(
            &writer(),
            moved(&old, &tree, "refs/heads/main"),
            &git_for(&s)
        ),
        FORCE
    );
}

#[test]
fn the_force_push_line_follows_the_refs_other_findings() {
    let s = Scratch::new();
    let (old, _) = rewritten(&s);
    s.write(".env", b"KEY=fixture\n");
    let new = s.stage_commit("rewrite with env");
    assert_eq!(
        scan_rows(
            &writer(),
            moved(&old, &new, "refs/heads/main"),
            &git_for(&s)
        ),
        [
            "refs/heads/main .env secret_env_file env_file",
            "refs/heads/main / safety_relaxation force_push",
        ]
    );
}

#[test]
fn a_failing_ancestry_check_fails_closed() {
    let s = Scratch::new();
    let (old, new) = rewritten(&s);
    let home = s.dir.path().to_str().unwrap().to_owned();
    let failing = s.script(
        "merge-base-fails",
        &format!(
            "[ \"$1\" = merge-base ] && exit 2\nHOME='{home}' GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 exec git \"$@\"\n"
        ),
    );
    let git = Git::new(failing, Some(s.bare.clone()));
    assert!(matches!(
        scan(&writer(), &[moved(&old, &new, "refs/heads/main")], &git),
        Err(ScanError::Git(GitCmdError::Failed {
            subcommand: "merge-base",
            ..
        }))
    ));
}

#[test]
fn a_sha256_default_branch_is_protected_the_same_way() {
    let s = sha256_scratch();
    let (old, new) = rewritten(&s);
    assert_eq!(old.len(), 64);
    let zero = "0".repeat(64);
    let git = git_for(&s);
    assert_eq!(
        scan_rows(&writer(), moved(&old, &new, "refs/heads/main"), &git),
        FORCE
    );
    assert_eq!(
        scan_rows(&writer(), moved(&old, &zero, "refs/heads/main"), &git),
        DELETE
    );
    s.write("a.txt", b"four\n");
    let ahead = s.stage_commit("ahead");
    assert!(scan_rows(&writer(), moved(&new, &ahead, "refs/heads/main"), &git).is_empty());
}

#[test]
fn an_accepted_rewrite_is_logged_with_workspace_pusher_override_and_rule() {
    let s = Scratch::new();
    let (old, new) = rewritten(&s);
    s.publish("main");
    let git = git_for(&s);
    let log = |u: RefUpdate, git: &Git| {
        crate::logs::captured_logs(|| {
            potluck::secret_check::log_default_branch_rewrites(
                7,
                "alice",
                Some(3),
                Some("main"),
                &[u],
                git,
            );
        })
    };
    for (update, rule) in [
        (moved(&old, &new, "refs/heads/main"), "force_push"),
        (moved(&new, ZERO, "refs/heads/main"), "branch_delete"),
    ] {
        let text = log(update, &git);
        let line = text
            .lines()
            .find(|l| l.contains("default branch rewritten"))
            .unwrap_or_else(|| panic!("{text}"));
        for needle in [
            " INFO ",
            "workspace_id=7",
            "alice",
            "override_id=3",
            "refs/heads/main",
            rule,
        ] {
            assert!(line.contains(needle), "{needle} in {line}");
        }
    }
    s.write("a.txt", b"four\n");
    let ahead = s.stage_commit("ahead");
    let text = log(moved(&new, &ahead, "refs/heads/main"), &git);
    assert!(
        !text.contains("default branch rewritten"),
        "a fast-forward is not logged: {text}"
    );
    let broken = Git::new(PathBuf::from("/nonexistent/git"), Some(s.bare.clone()));
    let text = log(moved(&new, &ahead, "refs/heads/main"), &broken);
    assert!(
        text.contains(" WARN ") && text.contains("could not re-check an accepted admin push"),
        "{text}"
    );
}
