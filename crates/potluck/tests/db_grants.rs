//! Grants, the highest-grant rule, and the armed-override lookup.

use potluck::db::accounts::{NewUser, UserRecord};
use potluck::db::audit::AuditCtx;
use potluck::db::grants::{GrantRecord, Subject};
use potluck::db::teams::TeamRecord;
use potluck::db::workspaces::{NewWorkspace, WorkspaceRecord};
use potluck::db::{Db, DbError};
use potluck::role::Role;
use potluck::slug::Slug;

use crate::common::migrated_db;

fn slug(s: &str) -> Slug {
    Slug::parse(s).unwrap()
}

fn user(name: &str) -> Subject {
    Subject::User(name.to_owned())
}

fn team(name: &str) -> Subject {
    Subject::Team(name.to_owned())
}

struct Fx {
    db: Db,
    core: TeamRecord,
    api: WorkspaceRecord,
    docs: WorkspaceRecord,
    bob: UserRecord,
    ann: UserRecord,
}

async fn fx() -> (tempfile::TempDir, Fx) {
    let (dir, db) = migrated_db().await;
    let core = db
        .create_team(&slug("core"), "Core", &AuditCtx::system())
        .await
        .unwrap();
    let api = db
        .create_workspace(
            NewWorkspace::new(core.id(), &slug("api"), "API", "apinata", "main"),
            &AuditCtx::system(),
        )
        .await
        .unwrap();
    let docs = db
        .create_workspace(
            NewWorkspace::new(core.id(), &slug("docs"), "Docs", "apinata", "main"),
            &AuditCtx::system(),
        )
        .await
        .unwrap();
    let bob = db
        .create_user(NewUser::new("bob", "Bob"), &AuditCtx::system())
        .await
        .unwrap();
    let ann = db
        .create_user(NewUser::new("ann", "Ann"), &AuditCtx::system())
        .await
        .unwrap();
    (
        dir,
        Fx {
            db,
            core,
            api,
            docs,
            bob,
            ann,
        },
    )
}

#[test]
fn role_parse_matches_as_str() {
    for role in [Role::Read, Role::Write, Role::Admin] {
        assert_eq!(Role::parse(role.as_str()), Some(role));
    }
    for bad in ["Admin", "owner", "", "read "] {
        assert_eq!(Role::parse(bad), None, "{bad:?}");
    }
    assert_eq!(
        (user("bob").kind(), user("bob").name(), team("core").kind()),
        ("user", "bob", "team")
    );
}

#[tokio::test]
async fn grants_are_upserted_listed_and_deleted() {
    let (_d, f) = fx().await;
    let g =
        f.db.put_grant(f.api.id(), user("bob"), Role::Read, &AuditCtx::system())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(g, GrantRecord::new(user("bob"), Role::Read));
    let g =
        f.db.put_grant(f.api.id(), user("bob"), Role::Write, &AuditCtx::system())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(g.role(), Role::Write, "a second put replaces the role");
    f.db.put_grant(f.api.id(), team("core"), Role::Read, &AuditCtx::system())
        .await
        .unwrap()
        .unwrap();
    f.db.put_grant(f.api.id(), user("ann"), Role::Admin, &AuditCtx::system())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        f.db.put_grant(f.api.id(), user("nobody"), Role::Read, &AuditCtx::system())
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        f.db.put_grant(f.api.id(), team("nope"), Role::Read, &AuditCtx::system())
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        f.db.list_grants(f.api.id()).await.unwrap(),
        [
            GrantRecord::new(team("core"), Role::Read),
            GrantRecord::new(user("ann"), Role::Admin),
            GrantRecord::new(user("bob"), Role::Write),
        ],
        "teams first, then users, by name"
    );
    assert!(
        f.db.list_grants(f.docs.id()).await.unwrap().is_empty(),
        "grants are per workspace"
    );
    assert!(
        f.db.delete_grant(f.api.id(), user("bob"), &AuditCtx::system())
            .await
            .unwrap()
    );
    assert!(
        !f.db
            .delete_grant(f.api.id(), user("bob"), &AuditCtx::system())
            .await
            .unwrap(),
        "gone"
    );
    assert!(
        !f.db
            .delete_grant(f.api.id(), user("nobody"), &AuditCtx::system())
            .await
            .unwrap(),
        "unknown subject"
    );
    assert_eq!(f.db.list_grants(f.api.id()).await.unwrap().len(), 2);
}

#[tokio::test]
async fn the_highest_grant_wins_and_membership_alone_grants_nothing() {
    let (_d, f) = fx().await;
    assert_eq!(
        f.db.role_for(f.api.id(), f.bob.id()).await.unwrap(),
        None,
        "no grant, no role"
    );
    f.db.add_member(f.core.id(), f.bob.id(), &AuditCtx::system())
        .await
        .unwrap();
    assert_eq!(
        f.db.role_for(f.api.id(), f.bob.id()).await.unwrap(),
        None,
        "membership of the owning team is not a grant"
    );
    f.db.put_grant(f.api.id(), team("core"), Role::Write, &AuditCtx::system())
        .await
        .unwrap();
    assert_eq!(
        f.db.role_for(f.api.id(), f.bob.id()).await.unwrap(),
        Some(Role::Write),
        "a team grant reaches its members"
    );
    assert_eq!(
        f.db.role_for(f.api.id(), f.ann.id()).await.unwrap(),
        None,
        "and nobody else"
    );
    f.db.put_grant(f.api.id(), user("bob"), Role::Read, &AuditCtx::system())
        .await
        .unwrap();
    assert_eq!(
        f.db.role_for(f.api.id(), f.bob.id()).await.unwrap(),
        Some(Role::Write),
        "the higher of user and team wins"
    );
    f.db.put_grant(f.api.id(), user("bob"), Role::Admin, &AuditCtx::system())
        .await
        .unwrap();
    assert_eq!(
        f.db.role_for(f.api.id(), f.bob.id()).await.unwrap(),
        Some(Role::Admin)
    );
    assert_eq!(
        f.db.role_for(f.docs.id(), f.bob.id()).await.unwrap(),
        None,
        "another workspace"
    );
    f.db.remove_member(f.core.id(), f.bob.id(), &AuditCtx::system())
        .await
        .unwrap();
    f.db.delete_grant(f.api.id(), user("bob"), &AuditCtx::system())
        .await
        .unwrap();
    assert_eq!(
        f.db.role_for(f.api.id(), f.bob.id()).await.unwrap(),
        None,
        "lost membership and grant: nothing left"
    );
}

#[tokio::test]
async fn workspaces_for_user_lists_held_roles_in_order() {
    let (_d, f) = fx().await;
    assert!(
        f.db.workspaces_for_user(f.bob.id())
            .await
            .unwrap()
            .is_empty()
    );
    f.db.put_grant(f.docs.id(), user("bob"), Role::Read, &AuditCtx::system())
        .await
        .unwrap();
    f.db.add_member(f.core.id(), f.bob.id(), &AuditCtx::system())
        .await
        .unwrap();
    f.db.put_grant(f.api.id(), team("core"), Role::Write, &AuditCtx::system())
        .await
        .unwrap();
    f.db.put_grant(f.api.id(), user("bob"), Role::Read, &AuditCtx::system())
        .await
        .unwrap();
    let held = f.db.workspaces_for_user(f.bob.id()).await.unwrap();
    let seen: Vec<(&str, Role)> = held
        .iter()
        .map(|h| (h.workspace().slug(), h.role()))
        .collect();
    assert_eq!(seen, [("api", Role::Write), ("docs", Role::Read)]);
    assert_eq!(held[0].workspace(), &f.api, "the full record comes along");
    assert!(
        f.db.workspaces_for_user(f.ann.id())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn deleting_a_workspace_takes_its_grants_and_overrides_with_it() {
    let (_d, f) = fx().await;
    f.db.put_grant(f.api.id(), user("bob"), Role::Read, &AuditCtx::system())
        .await
        .unwrap();
    let id =
        f.db.arm_override(f.api.id(), f.bob.id(), "import", "system")
            .await
            .unwrap();
    assert_eq!(
        f.db.armed_override(f.api.id(), f.bob.id()).await.unwrap(),
        Some(id)
    );
    assert_eq!(
        f.db.armed_override(f.api.id(), f.ann.id()).await.unwrap(),
        None,
        "armed for one user"
    );
    assert_eq!(
        f.db.armed_override(f.docs.id(), f.bob.id()).await.unwrap(),
        None,
        "on one workspace"
    );
    let second =
        f.db.arm_override(f.api.id(), f.bob.id(), "again", "ann")
            .await
            .unwrap();
    assert!(second > id);
    assert_eq!(
        f.db.armed_override(f.api.id(), f.bob.id()).await.unwrap(),
        Some(id),
        "the earliest armed override is the one a push consumes"
    );
    assert!(
        f.db.delete_workspace(f.api.id(), &AuditCtx::system())
            .await
            .unwrap()
    );
    assert!(f.db.list_grants(f.api.id()).await.unwrap().is_empty());
    assert_eq!(
        f.db.armed_override(f.api.id(), f.bob.id()).await.unwrap(),
        None
    );
    assert_eq!(f.db.role_for(f.api.id(), f.bob.id()).await.unwrap(), None);
}

/// A second connection on the same file, the way an out-of-band writer would hold it.
fn raw(dir: &tempfile::TempDir) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(dir.path().join("server.db")).unwrap();
    c.pragma_update(None, "foreign_keys", true).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    c
}

fn grant_rows(c: &rusqlite::Connection) -> i64 {
    c.query_row("SELECT count(*) FROM grants", [], |r| r.get(0))
        .unwrap()
}

#[tokio::test]
async fn deleting_a_user_takes_their_grants_with_it() {
    let (d, f) = fx().await;
    f.db.put_grant(f.api.id(), user("ann"), Role::Admin, &AuditCtx::system())
        .await
        .unwrap()
        .unwrap();
    f.db.put_grant(f.docs.id(), user("ann"), Role::Read, &AuditCtx::system())
        .await
        .unwrap()
        .unwrap();
    f.db.put_grant(f.api.id(), user("bob"), Role::Write, &AuditCtx::system())
        .await
        .unwrap()
        .unwrap();
    let c = raw(&d);
    c.execute("DELETE FROM users WHERE id = ?1", [f.ann.id()])
        .unwrap();
    assert_eq!(grant_rows(&c), 1, "only bob's grant is left");
    assert_eq!(
        f.db.list_grants(f.api.id()).await.unwrap(),
        [GrantRecord::new(user("bob"), Role::Write)]
    );
    // SQLite gives the freed highest id to the next user, who must not inherit ann's role.
    let cat =
        f.db.create_user(NewUser::new("cat", "Cat"), &AuditCtx::system())
            .await
            .unwrap();
    assert_eq!(cat.id(), f.ann.id(), "precondition: the id is reused");
    assert_eq!(f.db.role_for(f.api.id(), cat.id()).await.unwrap(), None);
}

#[tokio::test]
async fn deleting_a_team_takes_only_its_own_grants_with_it() {
    let (d, f) = fx().await;
    let ops =
        f.db.create_team(&slug("ops"), "Ops", &AuditCtx::system())
            .await
            .unwrap();
    assert_eq!(
        ops.id(),
        f.ann.id(),
        "precondition: a team and a user share an id"
    );
    f.db.put_grant(f.api.id(), team("ops"), Role::Write, &AuditCtx::system())
        .await
        .unwrap()
        .unwrap();
    f.db.put_grant(f.api.id(), team("core"), Role::Read, &AuditCtx::system())
        .await
        .unwrap()
        .unwrap();
    f.db.put_grant(f.api.id(), user("ann"), Role::Read, &AuditCtx::system())
        .await
        .unwrap()
        .unwrap();
    let c = raw(&d);
    c.execute("DELETE FROM teams WHERE id = ?1", [ops.id()])
        .unwrap();
    assert_eq!(
        grant_rows(&c),
        2,
        "the user with the same id keeps their grant"
    );
    assert_eq!(
        f.db.list_grants(f.api.id()).await.unwrap(),
        [
            GrantRecord::new(team("core"), Role::Read),
            GrantRecord::new(user("ann"), Role::Read),
        ]
    );
}

#[tokio::test]
async fn a_rolled_back_delete_keeps_the_grants() {
    let (d, f) = fx().await;
    f.db.put_grant(f.api.id(), user("ann"), Role::Admin, &AuditCtx::system())
        .await
        .unwrap()
        .unwrap();
    let mut c = raw(&d);
    let tx = c.transaction().unwrap();
    tx.execute("DELETE FROM users WHERE id = ?1", [f.ann.id()])
        .unwrap();
    assert_eq!(
        grant_rows(&tx),
        0,
        "the grant goes in the delete's own transaction"
    );
    tx.rollback().unwrap();
    assert_eq!(
        grant_rows(&c),
        1,
        "and comes back when that transaction rolls back"
    );
}

#[tokio::test]
async fn list_grants_skips_a_grant_whose_subject_is_gone() {
    let (d, f) = fx().await;
    f.db.put_grant(f.api.id(), user("bob"), Role::Read, &AuditCtx::system())
        .await
        .unwrap()
        .unwrap();
    raw(&d)
        .execute(
            "INSERT INTO grants (workspace_id, subject_kind, subject_id, role) \
             VALUES (?1, 'user', 999, 'admin'), (?1, 'team', 999, 'write')",
            [f.api.id()],
        )
        .unwrap();
    assert_eq!(
        f.db.list_grants(f.api.id()).await.unwrap(),
        [GrantRecord::new(user("bob"), Role::Read)],
        "the dangling user and team grants are skipped, not an error"
    );
}

/// Only a hand edit or a future migration can store a role outside read, write and admin; the
/// lookups refuse it instead of reading it as `read`, even beside a valid higher grant.
#[tokio::test]
async fn an_unknown_grant_role_is_an_error_not_read() {
    let (d, f) = fx().await;
    f.db.add_member(f.core.id(), f.bob.id(), &AuditCtx::system())
        .await
        .unwrap();
    f.db.put_grant(f.api.id(), team("core"), Role::Admin, &AuditCtx::system())
        .await
        .unwrap();
    let c = raw(&d);
    c.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    for ws in [f.api.id(), f.docs.id()] {
        c.execute(
            "INSERT INTO grants (workspace_id, subject_kind, subject_id, role) \
             VALUES (?1, 'user', ?2, 'owner')",
            [ws, f.bob.id()],
        )
        .unwrap();
    }
    drop(c);
    for (ws, what) in [(f.docs.id(), "alone"), (f.api.id(), "beside a team admin")] {
        let err = f.db.role_for(ws, f.bob.id()).await.expect_err(what);
        assert!(matches!(err, DbError::Sqlite(_)), "{what}: {err:?}");
        assert!(err.to_string().contains("role"), "{what}: {err}");
    }
    let err =
        f.db.workspaces_for_user(f.bob.id())
            .await
            .expect_err("held list");
    assert!(matches!(err, DbError::Sqlite(_)), "{err:?}");
    assert_eq!(
        f.db.role_for(f.api.id(), f.ann.id()).await.unwrap(),
        None,
        "a user the bad rows do not reach is unaffected"
    );
}
