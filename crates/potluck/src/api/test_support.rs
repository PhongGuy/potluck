//! Loopback fixtures for the unit tests under `api`, each a copy of its namesake in the
//! integration suite's `common` module, which unit tests cannot reach.

use std::net::SocketAddr;
use std::path::Path;

use super::AppState;
use crate::auth::password::Passwords;
use crate::auth::token::MintedToken;
use crate::config::{Config, ServeArgs};
use crate::db::Db;
use crate::db::accounts::{NewToken, NewUser, UserRecord};
use crate::db::audit::AuditCtx;

/// A `Config` for a server on an ephemeral loopback port with `dir` as its data directory.
pub(crate) fn config_for(dir: &Path) -> Config {
    #[derive(clap::Parser)]
    struct Wrap {
        #[command(flatten)]
        serve: ServeArgs,
    }
    let data = dir.to_str().expect("utf-8 temp path");
    let argv = [
        "potluck",
        "--listen",
        "127.0.0.1:0",
        "--data",
        data,
        "--public-url",
        "http://127.0.0.1:8080",
    ];
    let wrap = <Wrap as clap::Parser>::try_parse_from(argv).expect("flags parse");
    Config::try_from(wrap.serve).expect("test config")
}

/// argon2id at the minimum cost: a test login costs a millisecond, not a tenth of a second.
pub(crate) fn fast_passwords() -> Passwords {
    Passwords::with_cost(1024, 1, 1).expect("valid cost")
}

/// An `AppState` over a migrated database with fast hashing; the dir keeps the files alive.
pub(crate) async fn test_state() -> (tempfile::TempDir, Db, AppState) {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Db::open(&dir.path().join("server.db")).expect("open");
    db.migrate().await.expect("migrate");
    let state = AppState::new(db.clone(), config_for(dir.path())).with_passwords(fast_passwords());
    (dir, db, state)
}

/// A user whose display name is the username, with `password` hashed at test cost.
pub(crate) async fn create_user(
    db: &Db,
    username: &str,
    password: &str,
    server_admin: bool,
) -> UserRecord {
    let hash = fast_passwords().hash(password).expect("hash");
    db.create_user(
        NewUser::new(username, username)
            .password_hash(&hash)
            .server_admin(server_admin),
        &AuditCtx::system(),
    )
    .await
    .expect("create user")
}

/// Mints a token for `user` straight in the database and returns the secret.
pub(crate) async fn mint_token(
    db: &Db,
    user: &UserRecord,
    name: &str,
    expires_at: Option<&str>,
) -> String {
    let minted = MintedToken::mint().expect("rng");
    db.create_token(NewToken::new(
        user.id(),
        name,
        minted.prefix(),
        *minted.digest(),
        expires_at,
    ))
    .await
    .expect("create token");
    minted.secret().to_owned()
}

/// Serves `router` on an ephemeral loopback port for the life of the returned task.
pub(crate) async fn serve(router: axum::Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    (addr, task)
}
