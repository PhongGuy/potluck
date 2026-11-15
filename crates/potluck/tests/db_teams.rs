//! Team and membership rows.

use potluck::db::DbError;
use potluck::db::accounts::NewUser;
use potluck::db::teams::TeamRecord;
use potluck::slug::Slug;

use crate::common::migrated_db;

fn slug(s: &str) -> Slug {
    Slug::parse(s).unwrap()
}

#[tokio::test]
async fn teams_are_created_listed_and_found_by_slug() {
    let (_dir, db) = migrated_db().await;
    let core = db.create_team(&slug("core"), "Core").await.unwrap();
    assert_eq!((core.slug(), core.name()), ("core", "Core"));
    db.create_team(&slug("api"), "API").await.unwrap();
    let teams = db.list_teams().await.unwrap();
    assert_eq!(
        teams.iter().map(TeamRecord::slug).collect::<Vec<_>>(),
        ["api", "core"],
        "by slug"
    );
    assert_eq!(db.team_by_slug("core").await.unwrap(), Some(core.clone()));
    assert_eq!(db.team_by_slug("cor").await.unwrap(), None);
    let err = db.create_team(&slug("core"), "Again").await.unwrap_err();
    assert!(matches!(err, DbError::Conflict { .. }), "got {err:?}");
    assert!(err.to_string().contains("core"), "{err}");
    assert_eq!(
        db.list_teams().await.unwrap().len(),
        2,
        "the conflict inserted nothing"
    );
}

#[tokio::test]
async fn membership_is_idempotent_and_sorted() {
    let (_dir, db) = migrated_db().await;
    let team = db.create_team(&slug("core"), "Core").await.unwrap();
    let bob = db.create_user(NewUser::new("bob", "Bob")).await.unwrap();
    let ann = db.create_user(NewUser::new("ann", "Ann")).await.unwrap();
    assert!(db.add_member(team.id(), bob.id()).await.unwrap());
    assert!(db.add_member(team.id(), ann.id()).await.unwrap());
    assert!(
        !db.add_member(team.id(), bob.id()).await.unwrap(),
        "already a member"
    );
    assert_eq!(db.team_members(team.id()).await.unwrap(), ["ann", "bob"]);
    assert!(db.remove_member(team.id(), bob.id()).await.unwrap());
    assert!(
        !db.remove_member(team.id(), bob.id()).await.unwrap(),
        "not a member any more"
    );
    assert_eq!(db.team_members(team.id()).await.unwrap(), ["ann"]);
    assert!(
        db.team_members(999).await.unwrap().is_empty(),
        "an unknown team has no members"
    );
    let err = db.add_member(team.id(), 999).await.unwrap_err();
    assert!(
        matches!(err, DbError::Sqlite(_)),
        "an unknown user is a foreign key failure: {err:?}"
    );
}
