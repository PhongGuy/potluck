//! The deny-all resolver denies; the test resolver answers 401 and 404 the way the spec words
//! them and carries its hook variables.

use potluck::api::credential::Credential;
use potluck::git::access::{Access, DenyAll, GitAccess};
use potluck::role::Role;
use potluck::slug::Slug;

use crate::common::TestAccess;

fn slug(s: &str) -> Slug {
    Slug::parse(s).unwrap()
}

#[tokio::test]
async fn test_access_distinguishes_unknown_tokens_from_unreadable_workspaces() {
    let access = TestAccess::new()
        .workspace("core", "api")
        .workspace("core", "private")
        .grant("plk_w", "core", "api", Role::Write);
    let cred = Credential::new("plk_w".into());
    assert_eq!(
        access
            .resolve(&cred, &slug("core"), &slug("api"))
            .await
            .unwrap()
            .role(),
        Role::Write
    );
    assert_eq!(
        access
            .resolve(&cred, &slug("core"), &slug("private"))
            .await
            .unwrap_err()
            .status(),
        404,
        "known token, no grant"
    );
    assert_eq!(
        access
            .resolve(&cred, &slug("core"), &slug("nope"))
            .await
            .unwrap_err()
            .status(),
        404,
        "unknown workspace"
    );
    assert_eq!(
        access
            .resolve(
                &Credential::new("plk_x".into()),
                &slug("core"),
                &slug("api")
            )
            .await
            .unwrap_err()
            .status(),
        401,
        "unknown token"
    );
}

#[tokio::test]
async fn access_carries_hook_variables_in_order() {
    let plain = Access::new(Role::Read);
    assert_eq!(plain.role(), Role::Read);
    assert!(plain.hook_env().is_empty(), "nothing by default");
    let access = TestAccess::new()
        .workspace("core", "api")
        .grant("plk_w", "core", "api", Role::Write)
        .hook_env("POTLUCK_KIND", "apinata")
        .hook_env("POTLUCK_PUSHER", "ann");
    let granted = access
        .resolve(
            &Credential::new("plk_w".into()),
            &slug("core"),
            &slug("api"),
        )
        .await
        .unwrap();
    assert_eq!(
        granted.hook_env(),
        [
            ("POTLUCK_KIND".to_string(), "apinata".to_string()),
            ("POTLUCK_PUSHER".to_string(), "ann".to_string())
        ]
    );
}

#[tokio::test]
async fn deny_all_answers_unauthenticated() {
    let err = DenyAll
        .resolve(&Credential::new("plk_x".into()), &slug("t"), &slug("w"))
        .await
        .unwrap_err();
    assert_eq!(err.status(), 401);
}
