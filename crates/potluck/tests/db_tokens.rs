//! The token queries on the one database thread.

use potluck::db::Db;
use potluck::db::accounts::{NewToken, NewUser};

use crate::common::migrated_db;

fn digest(n: u8) -> [u8; 32] {
    [n; 32]
}

async fn last_used(db: &Db, id: i64) -> Option<String> {
    db.token_by_id(id)
        .await
        .unwrap()
        .unwrap()
        .last_used_at()
        .map(str::to_owned)
}

#[tokio::test]
async fn tokens_round_trip_by_prefix_user_and_id() {
    let (_dir, db) = migrated_db().await;
    let bob = db.create_user(NewUser::new("bob", "Bob")).await.unwrap();
    let t1 = db
        .create_token(NewToken::new(
            bob.id(),
            "laptop",
            "plk_aaaa",
            digest(1),
            None,
        ))
        .await
        .unwrap();
    let t2 = db
        .create_token(NewToken::new(
            bob.id(),
            "ci",
            "plk_aaaa",
            digest(2),
            Some("2030-01-01T00:00:00Z"),
        ))
        .await
        .unwrap();
    assert_eq!(t1.name(), "laptop");
    assert_eq!(t1.prefix(), "plk_aaaa");
    assert_eq!(t1.user_id(), bob.id());
    assert_eq!(t1.last_used_at(), None);
    assert_eq!(t1.expires_at(), None);
    assert_eq!(t1.created_at().len(), 20);
    assert_eq!(t2.expires_at(), Some("2030-01-01T00:00:00Z"));
    let candidates = db.tokens_by_prefix("plk_aaaa").await.unwrap();
    assert_eq!(candidates.len(), 2, "both share the prefix");
    let c1 = candidates
        .iter()
        .find(|c| c.token().id() == t1.id())
        .unwrap();
    assert_eq!(c1.digest(), &digest(1));
    assert_eq!(c1.user().username(), "bob");
    assert!(db.tokens_by_prefix("plk_zzzz").await.unwrap().is_empty());
    let ids: Vec<i64> = db
        .tokens_for_user(bob.id())
        .await
        .unwrap()
        .iter()
        .map(|t| t.id())
        .collect();
    assert_eq!(ids, [t1.id(), t2.id()], "creation order");
    assert_eq!(db.token_by_id(t2.id()).await.unwrap().unwrap().name(), "ci");
    assert!(db.token_by_id(9999).await.unwrap().is_none());
}

#[tokio::test]
async fn delete_token_reports_whether_a_row_went() {
    let (_dir, db) = migrated_db().await;
    let bob = db.create_user(NewUser::new("bob", "Bob")).await.unwrap();
    let t = db
        .create_token(NewToken::new(bob.id(), "x", "plk_aaaa", digest(3), None))
        .await
        .unwrap();
    assert!(db.delete_token(t.id()).await.unwrap());
    assert!(!db.delete_token(t.id()).await.unwrap());
    assert!(db.tokens_for_user(bob.id()).await.unwrap().is_empty());
    db.touch_token(t.id(), "2026-11-15T10:00:00Z", "2026-11-15T09:59:00Z")
        .await
        .expect("touching a revoked token is not an error");
}

#[tokio::test]
async fn touch_token_writes_only_when_stale() {
    let (_dir, db) = migrated_db().await;
    let bob = db.create_user(NewUser::new("bob", "Bob")).await.unwrap();
    let t = db
        .create_token(NewToken::new(bob.id(), "x", "plk_aaaa", digest(4), None))
        .await
        .unwrap();
    let id = t.id();
    db.touch_token(id, "2026-11-15T10:00:00Z", "2026-11-15T09:59:00Z")
        .await
        .unwrap();
    assert_eq!(
        last_used(&db, id).await.as_deref(),
        Some("2026-11-15T10:00:00Z"),
        "null is written"
    );
    db.touch_token(id, "2026-11-15T10:00:30Z", "2026-11-15T09:59:30Z")
        .await
        .unwrap();
    assert_eq!(
        last_used(&db, id).await.as_deref(),
        Some("2026-11-15T10:00:00Z"),
        "inside the window: untouched"
    );
    db.touch_token(id, "2026-11-15T10:02:00Z", "2026-11-15T10:01:00Z")
        .await
        .unwrap();
    assert_eq!(
        last_used(&db, id).await.as_deref(),
        Some("2026-11-15T10:02:00Z"),
        "stale: written"
    );
}

#[tokio::test]
async fn debug_redacts_digests() {
    let (_dir, db) = migrated_db().await;
    let bob = db
        .create_user(NewUser::new("bob", "Bob").password_hash("$argon2id$hashhash"))
        .await
        .unwrap();
    let new_token = NewToken::new(bob.id(), "x", "plk_aaaa", [0xAB; 32], None);
    let shown = format!("{new_token:?}");
    assert!(
        shown.contains("plk_aaaa") && shown.contains("redacted"),
        "{shown}"
    );
    assert!(
        !shown.contains("171, 171") && !shown.contains("0xab"),
        "NewToken leaked: {shown}"
    );
    db.create_token(new_token).await.unwrap();
    let candidates = db.tokens_by_prefix("plk_aaaa").await.unwrap();
    let shown = format!("{:?}", candidates[0]);
    assert!(
        shown.contains("plk_aaaa") && shown.contains("bob"),
        "{shown}"
    );
    assert!(
        !shown.contains("171, 171") && !shown.contains("hashhash"),
        "TokenCandidate leaked: {shown}"
    );
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `PartialEq` on a digest compares it byte by byte with an early exit, so no struct in `src/`
/// with a `digest` field may derive it.
#[test]
fn no_struct_with_a_digest_field_derives_partial_eq() {
    let mut files = Vec::new();
    rust_files(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let (mut seen, mut offenders) = (Vec::new(), Vec::new());
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            // A braced struct: its name, then its body up to the first closing-brace line.
            let decl = line.trim_start();
            let Some(rest) = ["pub struct ", "pub(crate) struct ", "struct "]
                .iter()
                .find_map(|p| decl.strip_prefix(p))
            else {
                continue;
            };
            if !decl.ends_with('{') {
                continue;
            }
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let has_digest = lines[i + 1..]
                .iter()
                .take_while(|l| l.trim() != "}")
                .any(|l| {
                    let f = l.trim_start();
                    let f = f
                        .strip_prefix("pub(crate) ")
                        .or(f.strip_prefix("pub "))
                        .unwrap_or(f);
                    f.starts_with("digest:")
                });
            if !has_digest {
                continue;
            }
            seen.push(name.clone());
            // The attributes and doc lines directly above the struct, minus the comments.
            let attrs: String = lines[..i]
                .iter()
                .rev()
                .take_while(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.ends_with('}') && !t.ends_with(';')
                })
                .filter(|l| !l.trim_start().starts_with("//"))
                .copied()
                .collect();
            if attrs.contains("PartialEq") {
                offenders.push(format!("{name} in {}", file.display()));
            }
        }
    }
    for expected in ["NewToken", "TokenCandidate", "MintedToken"] {
        assert!(
            seen.iter().any(|s| s == expected),
            "the scan missed {expected}: {seen:?}"
        );
    }
    assert!(
        offenders.is_empty(),
        "digest-bearing structs derive PartialEq: {offenders:?}"
    );
}
