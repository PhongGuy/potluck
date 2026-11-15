//! Shared helpers. Every test reaches only 127.0.0.1 or a Unix socket under a temp dir.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use clap::Parser;
use potluck::api::credential::Credential;
use potluck::api::error::ApiError;
use potluck::api::{AppState, router};
use potluck::auth::password::Passwords;
use potluck::auth::token::MintedToken;
use potluck::config::{Config, ConfigError, ServeArgs};
use potluck::db::Db;
use potluck::db::accounts::{NewToken, NewUser, UserRecord};
use potluck::git::access::{Access, BoxFuture, GitAccess};
use potluck::git::{GitLimits, Repos};
use potluck::role::Role;
use potluck::slug::Slug;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The built `potluck` binary with an empty environment, so no `POTLUCK_*` variable or
/// real config leaks in. Tests add what they need with `.env(..)`.
pub fn potluck() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_potluck"));
    cmd.env_clear();
    cmd
}

#[derive(Parser)]
struct ServeWrap {
    #[command(flatten)]
    serve: ServeArgs,
}

/// Builds a `Config` from `potluck serve` flags. Pass every flag explicitly: clap also reads
/// `POTLUCK_*` from the real environment, and these tests must not depend on it.
pub fn parse_config(args: &[&str]) -> Result<Config, ConfigError> {
    let argv = std::iter::once("potluck").chain(args.iter().copied());
    let wrap = ServeWrap::try_parse_from(argv).expect("flags parse");
    Config::try_from(wrap.serve)
}

/// A `Config` for a server on an ephemeral loopback port with `dir` as its data directory.
pub fn config_for(dir: &Path) -> Config {
    let data = dir.to_str().expect("utf-8 temp path");
    parse_config(&[
        "--listen",
        "127.0.0.1:0",
        "--data",
        data,
        "--public-url",
        "http://127.0.0.1:8080",
    ])
    .expect("test config")
}

/// Serves `router` on an ephemeral loopback port for the life of the returned task.
pub async fn serve(router: axum::Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    (addr, task)
}

/// A migrated database in its own temp dir.
pub async fn migrated_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Db::open(&dir.path().join("server.db")).expect("open");
    db.migrate().await.expect("migrate");
    (dir, db)
}

/// argon2id at the minimum cost: a test login costs a millisecond, not a tenth of a second.
pub fn fast_passwords() -> Passwords {
    Passwords::with_cost(1024, 1, 1).expect("valid cost")
}

/// An `AppState` over a migrated database with fast hashing; the dir keeps the files alive.
pub async fn test_state() -> (tempfile::TempDir, Db, AppState) {
    let (dir, db) = migrated_db().await;
    let state = AppState::new(db.clone(), config_for(dir.path())).with_passwords(fast_passwords());
    (dir, db, state)
}

/// A user whose display name is the username, with `password` hashed at test cost.
pub async fn create_user(
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
    )
    .await
    .expect("create user")
}

/// Mints a token for `user` straight in the database and returns the secret.
pub async fn mint_token(
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

/// One HTTP/1.1 GET over a Unix socket; returns the status code and the body.
pub async fn unix_get(socket: &Path, target: &str) -> (u16, String) {
    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .expect("connect admin socket");
    let request = format!("GET {target} HTTP/1.1\r\nHost: potluck\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut raw = String::new();
    stream
        .read_to_string(&mut raw)
        .await
        .expect("read response");
    let (head, body) = raw.split_once("\r\n\r\n").expect("header terminator");
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code");
    (status, body.to_string())
}

/// An in-memory resolver: known tokens, existing workspaces, grants per token and workspace,
/// and the hook variables every grant carries.
#[derive(Debug, Default, Clone)]
pub struct TestAccess {
    workspaces: HashSet<(String, String)>,
    grants: HashMap<(String, String, String), Role>,
    hook_env: Vec<(String, String)>,
}

impl TestAccess {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn workspace(mut self, team: &str, ws: &str) -> Self {
        self.workspaces.insert((team.into(), ws.into()));
        self
    }
    /// `token` becomes a known token with `role` on `{team}/{ws}`.
    pub fn grant(mut self, token: &str, team: &str, ws: &str, role: Role) -> Self {
        self.grants
            .insert((token.into(), team.into(), ws.into()), role);
        self
    }
    /// Every granted `Access` carries `key=value` for the hook.
    pub fn hook_env(mut self, key: &str, value: &str) -> Self {
        self.hook_env.push((key.into(), value.into()));
        self
    }
}

impl GitAccess for TestAccess {
    fn resolve<'a>(
        &'a self,
        credential: &'a Credential,
        team: &'a Slug,
        ws: &'a Slug,
    ) -> BoxFuture<'a, Result<Access, ApiError>> {
        Box::pin(async move {
            let known = self.grants.keys().any(|(t, _, _)| t == credential.secret());
            if !known {
                return Err(ApiError::unauthenticated("unknown token"));
            }
            if !self
                .workspaces
                .contains(&(team.to_string(), ws.to_string()))
            {
                return Err(ApiError::not_found("no such workspace"));
            }
            let role = self
                .grants
                .get(&(
                    credential.secret().to_owned(),
                    team.to_string(),
                    ws.to_string(),
                ))
                .ok_or_else(|| ApiError::not_found("no such workspace"))?;
            Ok(self
                .hook_env
                .iter()
                .fold(Access::new(*role), |a, (k, v)| a.with_hook_env(k, v)))
        })
    }
}

/// A served router with the git routes over a temp data dir and a `TestAccess`. The data dir
/// is `dir`, so the hooks dir is `dir.path().join("hooks")`. No field without a reader: this is
/// a test binary, where an unread field warns even when `pub`.
pub struct GitServer {
    pub dir: tempfile::TempDir,
    pub base: String,
    pub repos: Repos,
    _task: tokio::task::JoinHandle<()>,
}

impl GitServer {
    /// `{base}/git/{team}/{ws}.git`.
    pub fn url(&self, team: &str, ws: &str) -> String {
        format!("{}/git/{team}/{ws}.git", self.base)
    }
}

/// Starts the server with `dir` as `--data`; `extra` adds `potluck serve` flags such as
/// `--max-push-bytes`.
pub async fn git_server(
    access: TestAccess,
    limits: Option<GitLimits>,
    extra: &[&str],
) -> GitServer {
    let dir = tempfile::tempdir().expect("temp dir");
    let data = dir.path().to_str().expect("utf-8 temp path").to_owned();
    git_server_at(dir, &data, access, limits, extra).await
}

/// Like `git_server`, with `--data` given as `data_flag` (which may be relative) while `dir`
/// keeps the directory alive.
pub async fn git_server_at(
    dir: tempfile::TempDir,
    data_flag: &str,
    access: TestAccess,
    limits: Option<GitLimits>,
    extra: &[&str],
) -> GitServer {
    let mut args = vec![
        "--listen",
        "127.0.0.1:0",
        "--data",
        data_flag,
        "--public-url",
        "http://127.0.0.1:8080",
    ];
    args.extend_from_slice(extra);
    let config = parse_config(&args).expect("test config");
    for d in [config.hooks_dir(), config.repos_dir(), config.tmp_dir()] {
        std::fs::create_dir_all(d).expect("data subdir");
    }
    let db = Db::open(&config.db_path()).expect("open");
    db.migrate().await.expect("migrate");
    let repos = Repos::new(config.repos_dir(), config.git().to_path_buf());
    let mut state = AppState::new(db, config).with_git_access(Arc::new(access));
    if let Some(limits) = limits {
        state = state.with_git_limits(limits);
    }
    let (addr, task) = serve(router(state)).await;
    GitServer {
        dir,
        base: format!("http://{addr}"),
        repos,
        _task: task,
    }
}

/// A `git` command isolated from the developer's config and keyring, with the token (when given)
/// sent as a Bearer header through `GIT_CONFIG_*` variables, never through argv or the URL.
pub fn git(home: &Path, cwd: Option<&Path>, token: Option<&str>, args: &[&str]) -> Output {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid");
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    if let Some(token) = token {
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env(
                "GIT_CONFIG_VALUE_0",
                format!("Authorization: Bearer {token}"),
            );
    }
    cmd.output().expect("run git")
}

/// Panics with stderr when `out` failed; returns stdout.
pub fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Asserts failure and returns stdout followed by stderr: `push --porcelain` reports rejected
/// refs on stdout while curl's errors and `remote:` lines go to stderr.
pub fn failed(out: Output) -> String {
    assert!(
        !out.status.success(),
        "git unexpectedly succeeded: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A work tree with one commit on `main`.
pub fn work_tree(home: &Path, name: &str) -> std::path::PathBuf {
    let work = home.join(name);
    std::fs::create_dir_all(&work).unwrap();
    ok(git(home, Some(&work), None, &["init", "-q", "-b", "main"]));
    std::fs::write(work.join("README"), "hello\n").unwrap();
    ok(git(home, Some(&work), None, &["add", "README"]));
    ok(git(home, Some(&work), None, &["commit", "-q", "-m", "one"]));
    work
}
