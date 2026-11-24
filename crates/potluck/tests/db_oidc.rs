//! `oidc_links`: one subject per user per issuer, lookups by (issuer, subject), audited writes.

use potluck::db::DbError;
use potluck::db::audit::{AuditCtx, AuditFilter};

use crate::common::{create_user, migrated_db};

const ISS: &str = "https://idp.example.com/realms/acme";

#[tokio::test]
async fn the_schema_is_at_version_four() {
    let (_dir, db) = migrated_db().await;
    assert_eq!(db.schema_version().await.unwrap(), 4);
}

#[tokio::test]
async fn a_link_finds_its_user_only_under_its_issuer() {
    let (_dir, db) = migrated_db().await;
    let ada = create_user(&db, "ada", "pw-123456", false).await;
    let link = db
        .put_oidc_link(ISS, ada.id(), "sub-1", &AuditCtx::system())
        .await
        .unwrap();
    assert_eq!(
        (link.issuer(), link.subject(), link.user_id()),
        (ISS, "sub-1", ada.id())
    );
    assert_eq!(
        db.user_by_oidc(ISS, "sub-1")
            .await
            .unwrap()
            .unwrap()
            .username(),
        "ada"
    );
    assert!(
        db.user_by_oidc(&format!("{ISS}/"), "sub-1")
            .await
            .unwrap()
            .is_none(),
        "issuer is exact"
    );
    assert!(db.user_by_oidc(ISS, "sub-2").await.unwrap().is_none());
}

#[tokio::test]
async fn putting_again_replaces_the_users_link() {
    let (_dir, db) = migrated_db().await;
    let ada = create_user(&db, "ada", "pw-123456", false).await;
    db.put_oidc_link(ISS, ada.id(), "old", &AuditCtx::system())
        .await
        .unwrap();
    db.put_oidc_link(ISS, ada.id(), "new", &AuditCtx::system())
        .await
        .unwrap();
    assert!(db.user_by_oidc(ISS, "old").await.unwrap().is_none());
    assert_eq!(
        db.oidc_link_of(ISS, ada.id())
            .await
            .unwrap()
            .unwrap()
            .subject(),
        "new"
    );
}

#[tokio::test]
async fn a_subject_linked_to_another_user_conflicts() {
    let (_dir, db) = migrated_db().await;
    let ada = create_user(&db, "ada", "pw-123456", false).await;
    let bob = create_user(&db, "bob", "pw-123456", false).await;
    db.put_oidc_link(ISS, ada.id(), "sub-1", &AuditCtx::system())
        .await
        .unwrap();
    let err = db
        .put_oidc_link(ISS, bob.id(), "sub-1", &AuditCtx::system())
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Conflict { .. }), "{err:?}");
    assert_eq!(
        db.user_by_oidc(ISS, "sub-1")
            .await
            .unwrap()
            .unwrap()
            .username(),
        "ada"
    );
}

#[tokio::test]
async fn a_refused_link_keeps_the_users_old_link() {
    let (_dir, db) = migrated_db().await;
    let ada = create_user(&db, "ada", "pw-123456", false).await;
    let bob = create_user(&db, "bob", "pw-123456", false).await;
    db.put_oidc_link(ISS, ada.id(), "sub-1", &AuditCtx::system())
        .await
        .unwrap();
    db.put_oidc_link(ISS, bob.id(), "sub-b", &AuditCtx::system())
        .await
        .unwrap();
    let err = db
        .put_oidc_link(ISS, bob.id(), "sub-1", &AuditCtx::system())
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Conflict { .. }), "{err:?}");
    let kept = db
        .oidc_link_of(ISS, bob.id())
        .await
        .unwrap()
        .expect("bob keeps a link");
    assert_eq!(
        kept.subject(),
        "sub-b",
        "the refused put rolled back its delete"
    );
}

#[tokio::test]
async fn delete_reports_whether_a_link_went() {
    let (_dir, db) = migrated_db().await;
    let ada = create_user(&db, "ada", "pw-123456", false).await;
    assert!(
        !db.delete_oidc_link(ISS, ada.id(), &AuditCtx::system())
            .await
            .unwrap()
    );
    db.put_oidc_link(ISS, ada.id(), "sub-1", &AuditCtx::system())
        .await
        .unwrap();
    assert!(
        db.delete_oidc_link(ISS, ada.id(), &AuditCtx::system())
            .await
            .unwrap()
    );
    assert!(db.oidc_link_of(ISS, ada.id()).await.unwrap().is_none());
}

#[tokio::test]
async fn link_writes_carry_the_callers_audit_context() {
    let (_dir, db) = migrated_db().await;
    let ada = create_user(&db, "ada", "pw-123456", false).await;
    let ctx = AuditCtx::user(ada.id(), "ada").with_client_ip("127.0.0.1".parse().unwrap());
    db.put_oidc_link(ISS, ada.id(), "sub-1", &ctx)
        .await
        .unwrap();
    db.delete_oidc_link(ISS, ada.id(), &ctx).await.unwrap();
    for action in ["user.oidc_link", "user.oidc_unlink"] {
        let rows = db.audit(AuditFilter::new().action(action)).await.unwrap();
        assert_eq!(rows.len(), 1, "{action}");
        let row = &rows[0];
        assert_eq!(
            (row.actor_kind(), row.actor_id(), row.actor_name()),
            ("user", Some(ada.id()), Some("ada"))
        );
        assert_eq!(row.client_ip(), Some("127.0.0.1"));
        assert_eq!(
            (row.target_kind(), row.target_id(), row.target_name()),
            (Some("user"), Some(ada.id()), Some("ada"))
        );
        let detail: serde_json::Value = serde_json::from_str(row.detail().unwrap()).unwrap();
        assert_eq!(detail, serde_json::json!({"subject": "sub-1"}), "{action}");
    }
}

#[tokio::test]
async fn a_refused_link_and_an_absent_unlink_write_no_row() {
    let (_dir, db) = migrated_db().await;
    let ada = create_user(&db, "ada", "pw-123456", false).await;
    let bob = create_user(&db, "bob", "pw-123456", false).await;
    db.put_oidc_link(ISS, ada.id(), "sub-1", &AuditCtx::system())
        .await
        .unwrap();
    db.put_oidc_link(ISS, bob.id(), "sub-1", &AuditCtx::system())
        .await
        .unwrap_err();
    db.delete_oidc_link(ISS, bob.id(), &AuditCtx::system())
        .await
        .unwrap();
    let links = db
        .audit(AuditFilter::new().action("user.oidc_link"))
        .await
        .unwrap();
    assert_eq!(links.len(), 1, "only ada's link is recorded");
    assert!(
        db.audit(AuditFilter::new().action("user.oidc_unlink"))
            .await
            .unwrap()
            .is_empty()
    );
}
