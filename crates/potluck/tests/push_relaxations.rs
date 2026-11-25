//! Accepted pushes that relax a safety rule leave one `push.relaxation` audit row per ref.

use std::path::{Path, PathBuf};
use std::time::Duration;

use potluck::db::audit::{AuditFilter, AuditRecord};
use reqwest::Method;
use serde_json::{Value, json};

use crate::common::{
    FullServer, api_admin, api_team, api_user, api_workspace, call, failed, full_server, git, ok,
    write_script,
};

/// A full server running the real secret check, a seeded `core/api` on `main`, the admin
/// `root`'s token, writer `bob`'s token and a scratch home.
struct Arena {
    server: FullServer,
    root: String,
    bob: String,
    home: PathBuf,
}

async fn arena() -> Arena {
    let server = full_server().await;
    write_script(
        &server.hooks_dir().join("pre-receive"),
        &potluck::hooks::script(Path::new(env!("CARGO_BIN_EXE_potluck")), Path::new("git"))
            .unwrap(),
    );
    let root = api_admin(server.db()).await;
    let (_, bob) = api_user(server.db(), "bob").await;
    api_team(&server.api(""), &root, "core").await;
    api_workspace(&server.api(""), &root, "core", "api", true).await;
    let (status, _) = call(
        &server.api("/workspaces/core/api/grants/user/bob"),
        Method::PUT,
        Some(&root),
        Some(json!({"role": "write"})),
    )
    .await;
    assert_eq!(status, 200);
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    Arena {
        server,
        root,
        bob,
        home,
    }
}

impl Arena {
    /// A clone of `core/api` at `<home>/<name>` made with `token`.
    fn clone(&self, name: &str, token: &str) -> PathBuf {
        let url = self.server.git_url("core", "api");
        ok(git(
            &self.home,
            Some(&self.home),
            Some(token),
            &["clone", "-q", &url, name],
        ));
        self.home.join(name)
    }

    /// Runs git in `work` with `token`.
    fn git(&self, work: &Path, token: &str, args: &[&str]) -> std::process::Output {
        git(&self.home, Some(work), Some(token), args)
    }

    /// Writes `text` to `README` and commits it; returns the new HEAD.
    fn commit(&self, work: &Path, text: &str, amend: bool) -> String {
        std::fs::write(work.join("README"), text).unwrap();
        ok(git(&self.home, Some(work), None, &["add", "README"]));
        let mut args = vec!["commit", "-q", "-m", text];
        if amend {
            args.push("--amend");
        }
        ok(git(&self.home, Some(work), None, &args));
        self.head(work, "HEAD")
    }

    fn head(&self, work: &Path, rev: &str) -> String {
        ok(git(&self.home, Some(work), None, &["rev-parse", rev]))
            .trim()
            .to_owned()
    }

    /// The `push.relaxation` rows, oldest first.
    async fn rows(&self) -> Vec<AuditRecord> {
        let mut rows = self
            .server
            .db()
            .audit(AuditFilter::new().action("push.relaxation").limit(100))
            .await
            .unwrap();
        rows.reverse();
        rows
    }

    /// The rows once at least `n` have landed (the recheck runs after git exits), after a
    /// further pause that lets a stray extra row land too.
    async fn rows_after(&self, n: usize) -> Vec<AuditRecord> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.rows().await.len() < n {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the relaxation rows land within 10 s");
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.rows().await
    }
}

fn detail(row: &AuditRecord) -> Value {
    serde_json::from_str(row.detail().expect("a detail")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admin_force_push_to_main_writes_one_row_with_the_old_sha() {
    let a = arena().await;
    let work = a.clone("work", &a.root);
    let old = a.commit(&work, "two\n", false);
    ok(a.git(&work, &a.root, &["push", "-q", "origin", "main"]));
    let new = a.commit(&work, "two, rewritten\n", true);
    ok(a.git(&work, &a.root, &["push", "-q", "--force", "origin", "main"]));
    let rows = a.rows_after(1).await;
    assert_eq!(rows.len(), 1, "only the rewrite is a relaxation");
    let row = &rows[0];
    assert_eq!(
        (row.actor_kind(), row.actor_name(), row.target_kind()),
        ("user", Some("root"), Some("workspace"))
    );
    assert_eq!(
        (row.target_name(), row.workspace()),
        (Some("core/api"), Some("core/api"))
    );
    assert!(row.actor_id().is_some() && row.target_id().is_some());
    assert_eq!(
        detail(row),
        json!({"ref": "refs/heads/main", "old": old, "new": new, "rule": "force_push", "override": null})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_override_backed_force_push_names_the_override() {
    let a = arena().await;
    let work = a.clone("work", &a.bob);
    let old = a.commit(&work, "two\n", false);
    ok(a.git(&work, &a.bob, &["push", "-q", "origin", "main"]));
    let (status, ov) = call(
        &a.server.api("/workspaces/core/api/overrides"),
        Method::POST,
        Some(&a.root),
        Some(json!({"for_user": "bob", "reason": "rewrite"})),
    )
    .await;
    assert_eq!(status, 201, "{ov}");
    let id = ov["id"].as_i64().unwrap();
    let new = a.commit(&work, "two, rewritten\n", true);
    ok(a.git(&work, &a.bob, &["push", "-q", "--force", "origin", "main"]));
    let rows = a.rows_after(1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].actor_name(), Some("bob"));
    assert_eq!(
        detail(&rows[0]),
        json!({"ref": "refs/heads/main", "old": old, "new": new, "rule": "force_push", "override": id})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admin_tag_move_writes_a_tag_move_row() {
    let a = arena().await;
    let work = a.clone("work", &a.root);
    ok(a.git(&work, &a.root, &["tag", "v1"]));
    ok(a.git(&work, &a.root, &["push", "-q", "origin", "v1"]));
    let old = a.head(&work, "v1");
    let new = a.commit(&work, "two\n", false);
    ok(a.git(&work, &a.root, &["tag", "-f", "v1"]));
    ok(a.git(
        &work,
        &a.root,
        &["push", "-q", "--force", "origin", "refs/tags/v1"],
    ));
    let rows = a.rows_after(1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        detail(&rows[0]),
        json!({"ref": "refs/tags/v1", "old": old, "new": new, "rule": "tag_move", "override": null})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_writer_fast_forward_and_a_refused_force_push_write_no_row() {
    let a = arena().await;
    let work = a.clone("work", &a.bob);
    a.commit(&work, "two\n", false);
    ok(a.git(&work, &a.bob, &["push", "-q", "origin", "main"]));
    a.commit(&work, "two, rewritten\n", true);
    let out = failed(a.git(&work, &a.bob, &["push", "--force", "origin", "main"]));
    assert!(out.contains("force_push"), "{out}");
    // Neither push may write a row, so give a stray one time to land before looking.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(a.rows().await.is_empty());
}
