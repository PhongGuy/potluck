//! An accepted push consumes the armed override and clears the import state in one go.

use std::time::Duration;

use potluck::db::workspaces::NewWorkspace;
use potluck::git::access::PushContext;
use potluck::git::push::{RefMove, ZERO_SHA};
use potluck::role::Role;
use potluck::slug::Slug;
use tokio::time::timeout;

use crate::common::{
    GitServer, TestAccess, create_user, failed, git, git_server, ok, work_tree, write_hook,
};

const TOKEN: &str = "plk_writer_token_000000000000000000000000000";

/// A server whose `core/api` row is in import state with an override armed for `bob`, and whose
/// resolver carries the matching context and hook variables; returns `(server, ws_id, override_id)`.
async fn import_server(with_override: bool) -> (GitServer, i64, i64) {
    let access =
        TestAccess::new()
            .workspace("core", "api")
            .grant(TOKEN, "core", "api", Role::Write);
    let server = git_server(access, None, &[]).await;
    let bob = create_user(&server.db, "bob", "pw-123456", false).await;
    let team = server
        .db
        .create_team(&Slug::parse("core").unwrap(), "Core")
        .await
        .unwrap();
    let ws = server
        .db
        .create_workspace(
            NewWorkspace::new(
                team.id(),
                &Slug::parse("api").unwrap(),
                "API",
                "apinata",
                "main",
            )
            .import_pending(true),
        )
        .await
        .unwrap();
    let id = server
        .db
        .arm_override(ws.id(), bob.id(), "import", "system")
        .await
        .unwrap();
    server
        .repos
        .init(
            &Slug::parse("core").unwrap(),
            &Slug::parse("api").unwrap(),
            "main",
        )
        .await
        .unwrap();
    let mut ctx = PushContext::new(ws.id(), "bob").import(true);
    let mut access = TestAccess::new()
        .workspace("core", "api")
        .grant(TOKEN, "core", "api", Role::Write)
        .hook_env("POTLUCK_IMPORT", "1");
    if with_override {
        ctx = ctx.override_id(Some(id));
        access = access.hook_env("POTLUCK_OVERRIDE", &id.to_string());
    }
    let server = crate::common::git_server_with(server, access.push_context(ctx)).await;
    (server, ws.id(), id)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_import_push_with_an_override_consumes_it_and_clears_the_import_state() {
    let (server, ws, id) = import_server(true).await;
    let mut sub = server.notices.subscribe(ws, 1, 1);
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    write_hook(
        &server.dir.path().join("hooks"),
        "[ -n \"$POTLUCK_OVERRIDE\" ] || exit 1",
    );
    let work = work_tree(&home, "work");
    let tip = ok(git(&home, Some(&work), None, &["rev-parse", "HEAD"]))
        .trim()
        .to_owned();
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &[
            "push",
            "-q",
            &server.url("core", "api"),
            "main",
            "main:refs/heads/topic",
        ],
    ));
    let expected = vec![
        RefMove::new("refs/heads/main", ZERO_SHA, &tip),
        RefMove::new("refs/heads/topic", ZERO_SHA, &tip),
    ];
    let mut got = vec![
        timeout(Duration::from_secs(10), sub.next())
            .await
            .unwrap()
            .unwrap(),
        timeout(Duration::from_secs(10), sub.next())
            .await
            .unwrap()
            .unwrap(),
    ];
    got.sort_by(|a, b| a.ref_name().cmp(b.ref_name()));
    assert_eq!(got, expected);
    let row = timeout(Duration::from_secs(10), async {
        loop {
            let row = server.db.override_by_id(ws, id).await.unwrap().unwrap();
            if row.consumed_at().is_some() {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the record lands");
    let mut refs = row.consumed_refs().unwrap().to_vec();
    refs.sort_by(|a, b| a.ref_name().cmp(b.ref_name()));
    assert_eq!(refs, expected, "every moved ref is in the audit row");
    assert!(
        !server
            .db
            .workspace_by_slugs("core", "api")
            .await
            .unwrap()
            .unwrap()
            .import_pending(),
        "the first accepted push ends the import"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declined_push_records_nothing() {
    let (server, ws, id) = import_server(false).await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    write_hook(
        &server.dir.path().join("hooks"),
        "[ -n \"$POTLUCK_OVERRIDE\" ] || exit 1",
    );
    let work = work_tree(&home, "work");
    let out = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "--porcelain", &server.url("core", "api"), "main"],
    ));
    assert!(out.contains("pre-receive hook declined"), "{out}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        server
            .db
            .override_by_id(ws, id)
            .await
            .unwrap()
            .unwrap()
            .consumed_at()
            .is_none(),
        "the override stays armed"
    );
    assert!(
        server
            .db
            .workspace_by_slugs("core", "api")
            .await
            .unwrap()
            .unwrap()
            .import_pending(),
        "still an import"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn notices_go_out_even_when_the_database_is_closed() {
    let (server, ws, _) = import_server(true).await;
    let mut sub = server.notices.subscribe(ws, 1, 1);
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = work_tree(&home, "work");
    server.db.close();
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &server.url("core", "api"), "main"],
    ));
    let m = timeout(Duration::from_secs(10), sub.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        m.ref_name(),
        "refs/heads/main",
        "the record failed, the notice still went"
    );
}
