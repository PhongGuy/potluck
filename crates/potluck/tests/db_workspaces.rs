//! Workspace rows: creation, lookup, listing and deletion.

use potluck::db::DbError;
use potluck::db::workspaces::NewWorkspace;
use potluck::slug::Slug;

use crate::common::migrated_db;

fn slug(s: &str) -> Slug {
    Slug::parse(s).unwrap()
}

#[tokio::test]
async fn workspaces_are_created_found_listed_and_deleted() {
    let (_dir, db) = migrated_db().await;
    let core = db.create_team(&slug("core"), "Core").await.unwrap();
    let other = db.create_team(&slug("other"), "Other").await.unwrap();
    let api = db
        .create_workspace(NewWorkspace::new(
            core.id(),
            &slug("api"),
            "API",
            "apinata",
            "main",
        ))
        .await
        .unwrap();
    assert_eq!(
        (
            api.team_id(),
            api.team_slug(),
            api.slug(),
            api.name(),
            api.kind(),
            api.default_branch(),
            api.import_pending()
        ),
        (core.id(), "core", "api", "API", "apinata", "main", false)
    );
    assert_eq!(api.created_at().len(), 20);
    let imported = db
        .create_workspace(
            NewWorkspace::new(other.id(), &slug("api"), "API", "apinata", "trunk")
                .import_pending(true),
        )
        .await
        .unwrap();
    assert!(imported.import_pending());
    assert_eq!(imported.default_branch(), "trunk");
    assert_ne!(
        api.id(),
        imported.id(),
        "the same slug under another team is another workspace"
    );
    assert_eq!(
        db.workspace_by_slugs("core", "api").await.unwrap(),
        Some(api.clone())
    );
    assert_eq!(db.workspace_by_slugs("core", "nope").await.unwrap(), None);
    assert_eq!(db.workspace_by_slugs("nope", "api").await.unwrap(), None);
    let names: Vec<String> = db
        .list_workspaces()
        .await
        .unwrap()
        .iter()
        .map(|w| format!("{}/{}", w.team_slug(), w.slug()))
        .collect();
    assert_eq!(names, ["core/api", "other/api"]);
    let err = db
        .create_workspace(NewWorkspace::new(
            core.id(),
            &slug("api"),
            "Again",
            "apinata",
            "main",
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Conflict { .. }), "got {err:?}");
    assert!(db.delete_workspace(api.id()).await.unwrap());
    assert!(
        !db.delete_workspace(api.id()).await.unwrap(),
        "already gone"
    );
    assert_eq!(db.workspace_by_slugs("core", "api").await.unwrap(), None);
    assert_eq!(db.list_workspaces().await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_schema_refuses_an_unknown_kind_and_an_unknown_team() {
    let (_dir, db) = migrated_db().await;
    let core = db.create_team(&slug("core"), "Core").await.unwrap();
    let err = db
        .create_workspace(NewWorkspace::new(
            core.id(),
            &slug("x"),
            "X",
            "unknown",
            "main",
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DbError::Sqlite(_)),
        "the CHECK constraint holds: {err:?}"
    );
    let err = db
        .create_workspace(NewWorkspace::new(999, &slug("x"), "X", "apinata", "main"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DbError::Sqlite(_)),
        "the foreign key holds: {err:?}"
    );
    assert!(db.list_workspaces().await.unwrap().is_empty());
}
