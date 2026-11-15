//! The receive-pack command parser, `for-each-ref`, and which commands count as moves.

use std::collections::BTreeMap;
use std::path::PathBuf;

use potluck::git::Repos;
use potluck::git::push::{
    CommandError, MAX_COMMANDS, RefCommand, RefMove, ZERO_SHA, list_refs, moved, parse_commands,
};
use potluck::slug::Slug;

use crate::common::{git, ok, work_tree};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn pkt(line: &str) -> Vec<u8> {
    let mut out = format!("{:04x}", line.len() + 4).into_bytes();
    out.extend_from_slice(line.as_bytes());
    out
}

#[test]
fn parses_commands_and_skips_shallow_and_capabilities() {
    let mut body = pkt(&format!("shallow {A}"));
    body.extend(pkt(&format!(
        "{ZERO_SHA} {A} refs/heads/main\0report-status side-band-64k\n"
    )));
    body.extend(pkt(&format!("{A} {B} refs/heads/topic\n")));
    body.extend(pkt(&format!("{B} {ZERO_SHA} refs/tags/v1")));
    body.extend_from_slice(b"0000PACK garbage that must not be read");
    let commands = parse_commands(body.as_slice()).unwrap();
    assert_eq!(
        commands,
        vec![
            RefCommand::new(ZERO_SHA, A, "refs/heads/main"),
            RefCommand::new(A, B, "refs/heads/topic"),
            RefCommand::new(B, ZERO_SHA, "refs/tags/v1"),
        ]
    );
    assert_eq!(commands[0].new_sha(), A);
    assert_eq!(
        parse_commands(b"0000".as_slice()).unwrap(),
        vec![],
        "a flush alone is no commands"
    );
}

#[test]
fn malformed_and_truncated_lists_are_errors() {
    let mut body = pkt(&format!("{ZERO_SHA} {A} refs/heads/main\n"));
    body.extend(pkt("hello world\n"));
    body.extend_from_slice(b"0000");
    let err = parse_commands(body.as_slice()).unwrap_err();
    assert!(
        matches!(err, CommandError::Malformed { line: 2 }),
        "got {err:?}"
    );
    let err = parse_commands(pkt("zzzz not hex").as_slice()).unwrap_err();
    assert!(
        matches!(err, CommandError::Malformed { line: 1 }),
        "got {err:?}"
    );
    let err =
        parse_commands(pkt(&format!("{ZERO_SHA} {A} refs/heads/main")).as_slice()).unwrap_err();
    assert!(matches!(err, CommandError::Truncated), "no flush: {err:?}");
    let err = parse_commands(pkt(&format!("{ZERO_SHA} {A} ")).as_slice()).unwrap_err();
    assert!(
        matches!(err, CommandError::Malformed { line: 1 }),
        "empty ref: {err:?}"
    );
}

#[test]
fn a_list_over_the_cap_is_too_many_and_one_at_the_cap_is_not() {
    let list = |n: usize| -> Vec<u8> {
        let mut body: Vec<u8> = (0..n)
            .flat_map(|i| pkt(&format!("{ZERO_SHA} {A} refs/heads/b{i}\n")))
            .collect();
        body.extend_from_slice(b"0000");
        body
    };
    assert_eq!(
        parse_commands(list(MAX_COMMANDS).as_slice()).unwrap().len(),
        MAX_COMMANDS,
        "the cap itself is allowed"
    );
    let mut over = list(MAX_COMMANDS + 1);
    over.truncate(over.len() - 4);
    let err = parse_commands(over.as_slice()).unwrap_err();
    assert!(
        matches!(err, CommandError::TooMany { limit } if limit == MAX_COMMANDS),
        "got {err:?}; the flush is never reached"
    );
}

#[test]
fn moved_keeps_only_the_commands_the_repository_confirms() {
    let refs = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(r, s)| ((*r).to_owned(), (*s).to_owned()))
            .collect()
    };
    let before = refs(&[("refs/heads/gone", A), ("refs/heads/other", B)]);
    let after = refs(&[("refs/heads/main", A), ("refs/heads/other", B)]);
    let commands = [
        RefCommand::new(ZERO_SHA, A, "refs/heads/main"),
        RefCommand::new(ZERO_SHA, B, "refs/heads/topic"),
        RefCommand::new(A, ZERO_SHA, "refs/heads/gone"),
        RefCommand::new(B, ZERO_SHA, "refs/heads/other"),
        RefCommand::new(A, A, "refs/heads/main"),
        RefCommand::new(B, ZERO_SHA, "refs/heads/never"),
        RefCommand::new(ZERO_SHA, B, "refs/heads/other"),
    ];
    assert_eq!(
        moved(&commands, &before, &after),
        vec![
            RefMove::new("refs/heads/main", ZERO_SHA, A),
            RefMove::new("refs/heads/gone", A, ZERO_SHA),
        ],
        "a confirmed create, a confirmed deletion; not an unconfirmed create, a still-present ref, a no-op, a stale delete of a ref that was never there, or a create of a ref already at `new`"
    );
    assert_eq!(
        serde_json::to_string(&RefMove::new("r", A, B)).unwrap(),
        format!("{{\"ref\":\"r\",\"old\":\"{A}\",\"new\":\"{B}\"}}")
    );
}

#[tokio::test]
async fn list_refs_reads_a_bare_repository() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let repos = Repos::new(dir.path().join("repos"), PathBuf::from("git"));
    let bare = repos
        .init(
            &Slug::parse("core").unwrap(),
            &Slug::parse("api").unwrap(),
            "main",
        )
        .await
        .unwrap();
    assert_eq!(
        list_refs(repos.git(), &bare).await.unwrap(),
        BTreeMap::new(),
        "fresh repository"
    );
    let work = work_tree(&home, "work");
    let tip = ok(git(&home, Some(&work), None, &["rev-parse", "HEAD"]))
        .trim()
        .to_owned();
    ok(git(
        &home,
        Some(&home),
        None,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "fetch",
            "-q",
            work.to_str().unwrap(),
            "main:main",
            "main:refs/tags/v1",
        ],
    ));
    let refs = list_refs(repos.git(), &bare).await.unwrap();
    assert_eq!(refs.get("refs/heads/main"), Some(&tip));
    assert_eq!(refs.get("refs/tags/v1"), Some(&tip));
    assert_eq!(refs.len(), 2);
    let err = list_refs(repos.git(), &dir.path().join("nope.git"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, potluck::git::GitError::Failed { .. }),
        "got {err:?}"
    );
}
