//! Shared helpers. Every test reaches only 127.0.0.1 or a Unix socket under a temp dir.
pub mod scratch;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
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
use potluck::git::access::{Access, BoxFuture, GitAccess, PushContext};
use potluck::git::{GitLimits, Repos};
use potluck::notices::Notices;
use potluck::role::Role;
use potluck::server::{Running, start};
use potluck::slug::Slug;
use reqwest::Method;
use serde_json::Value;
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

/// A plain HTTP client for requests to a test server.
pub fn client() -> reqwest::Client {
    reqwest::Client::new()
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

/// One request to an absolute URL: `token` as Bearer, `body` as JSON. Returns the status and the
/// body (`Null` when empty).
pub async fn call(
    url: &str,
    method: Method,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let mut req = client().request(method, url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let r = req.send().await.expect("request");
    let status = r.status().as_u16();
    let text = r.text().await.expect("body");
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).expect("json body")
    };
    (status, value)
}

/// The server admin `root` with a token.
pub async fn api_admin(db: &Db) -> String {
    let root = create_user(db, "root", "pw-123456", true).await;
    mint_token(db, &root, "root", None).await
}

/// A plain user with a token.
pub async fn api_user(db: &Db, name: &str) -> (UserRecord, String) {
    let user = create_user(db, name, "pw-123456", false).await;
    let token = mint_token(db, &user, "laptop", None).await;
    (user, token)
}

/// `POST {api}/teams` as `token`; asserts 201. `api` is the `/api/v1` root, such as `app.url("")`.
pub async fn api_team(api: &str, token: &str, slug: &str) {
    let (status, body) = call(
        &format!("{api}/teams"),
        Method::POST,
        Some(token),
        Some(serde_json::json!({"slug": slug, "name": slug})),
    )
    .await;
    assert_eq!(status, 201, "team {slug}: {body}");
}

/// `POST {api}/teams/{team}/workspaces` for an apinata workspace on `main` as `token`; asserts
/// 201 and returns the Workspace object.
pub async fn api_workspace(api: &str, token: &str, team: &str, slug: &str, seed: bool) -> Value {
    let body = serde_json::json!({"slug": slug, "name": slug, "kind": "apinata", "default_branch": "main", "seed": seed});
    let (status, ws) = call(
        &format!("{api}/teams/{team}/workspaces"),
        Method::POST,
        Some(token),
        Some(body),
    )
    .await;
    assert_eq!(status, 201, "workspace {team}/{slug}: {ws}");
    ws
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
    push: Option<PushContext>,
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
    /// Every granted `Access` carries `push` (so pushes are recorded against it).
    pub fn push_context(mut self, push: PushContext) -> Self {
        self.push = Some(push);
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
            let access = self
                .hook_env
                .iter()
                .fold(Access::new(*role), |a, (k, v)| a.with_hook_env(k, v));
            Ok(match &self.push {
                Some(ctx) => access.with_push(ctx.clone()),
                None => access,
            })
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
    pub notices: Notices,
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
    potluck::hooks::install(
        &config.hooks_dir(),
        std::path::Path::new(env!("CARGO_BIN_EXE_potluck")),
        config.git(),
    )
    .expect("install hook");
    let db = Db::open(&config.db_path()).expect("open");
    db.migrate().await.expect("migrate");
    let repos = Repos::new(config.repos_dir(), config.git().to_path_buf());
    let mut state = AppState::new(db, config).with_git_access(Arc::new(access));
    if let Some(limits) = limits {
        state = state.with_git_limits(limits);
    }
    let notices = state.notices().clone();
    let (addr, task) = serve(router(state)).await;
    GitServer {
        dir,
        base: format!("http://{addr}"),
        repos,
        notices,
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

/// `<data>/repos/{team}/{ws}.git`.
pub fn repo_path(data: &Path, team: &str, ws: &str) -> std::path::PathBuf {
    data.join("repos").join(team).join(format!("{ws}.git"))
}

/// `<dir>/pre-receive` as `#!/bin/sh` plus `body`, mode 0755, over whatever is there.
pub fn write_hook(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir).expect("hooks dir");
    let hook = dir.join("pre-receive");
    std::fs::write(&hook, format!("#!/bin/sh\n{body}\n")).expect("write hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod hook");
}

/// A server from `server::start` on a temp data dir: directories made, database migrated, TCP
/// on an ephemeral loopback port, and an accepting `pre-receive` in place of the installed one.
/// Dropping it stops the listeners with the runtime.
pub struct FullServer {
    pub dir: tempfile::TempDir,
    pub running: Running,
}

impl FullServer {
    pub fn db(&self) -> &Db {
        self.running.db()
    }

    /// `http://127.0.0.1:<port>/api/v1{path}`.
    pub fn api(&self, path: &str) -> String {
        format!("http://{}/api/v1{path}", self.running.http_addr())
    }

    /// `http://127.0.0.1:<port>/git/{team}/{ws}.git`.
    pub fn git_url(&self, team: &str, ws: &str) -> String {
        format!("http://{}/git/{team}/{ws}.git", self.running.http_addr())
    }

    /// `<data>/hooks`.
    pub fn hooks_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("hooks")
    }
}

pub async fn full_server() -> FullServer {
    let dir = tempfile::tempdir().expect("temp dir");
    let running = start(&config_for(dir.path())).await.expect("start");
    let server = FullServer { dir, running };
    // The installed hook execs the serving binary, which inside this test binary is not
    // potluck; these tests are about roles, so an accepting stub stands in for the check.
    write_hook(&server.hooks_dir(), "exit 0");
    server
}

/// A served API over a migrated database with fast hashing; the dir and the task live as long as it.
pub struct TestApp {
    pub _dir: tempfile::TempDir,
    pub db: Db,
    pub addr: SocketAddr,
    pub _task: tokio::task::JoinHandle<()>,
}

impl TestApp {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}/api/v1{path}", self.addr)
    }
}

pub async fn test_app() -> TestApp {
    let (dir, db, state) = test_state().await;
    let (addr, task) = serve(potluck::api::router(state)).await;
    TestApp {
        _dir: dir,
        db,
        addr,
        _task: task,
    }
}

/// `POST /session` for an existing user; returns the minted token (named `test`).
pub async fn login(app: &TestApp, username: &str, password: &str) -> String {
    let r = client()
        .post(app.url("/session"))
        .json(
            &serde_json::json!({"username": username, "password": password, "token_name": "test"}),
        )
        .send()
        .await
        .expect("session request");
    assert_eq!(r.status().as_u16(), 201, "login as {username}");
    let body: serde_json::Value = r.json().await.expect("json");
    body["token"].as_str().expect("token").to_owned()
}

/// The repository root, two levels above this crate's manifest.
pub fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The jobs of a workflow as `(name, text)` pairs: a job is a key indented two spaces under
/// the column-0 `jobs:` key, and its text runs to the next such key or the next column-0 key.
/// A blank line or a column-0 comment inside a job does not end it.
pub fn jobs(workflow: &str) -> Vec<(String, String)> {
    // A line-shaped walk rather than a YAML parse: the workflows are ours and every invariant
    // the tests check is line-shaped, so a YAML crate would exist only for this.
    let mut jobs: Vec<(String, String)> = Vec::new();
    let mut in_jobs = false;
    for line in workflow.lines() {
        if line == "jobs:" {
            in_jobs = true;
            continue;
        }
        if !in_jobs {
            continue;
        }
        let column0_key = !line.is_empty() && !line.starts_with(' ') && !line.starts_with('#');
        if column0_key {
            break;
        }
        let job_key = line.strip_prefix("  ").and_then(|rest| {
            let name = rest.strip_suffix(':')?;
            let ident = !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            ident.then_some(name)
        });
        if let Some(name) = job_key {
            jobs.push((name.to_owned(), String::new()));
        }
        if let Some((_, text)) = jobs.last_mut() {
            text.push_str(line);
            text.push('\n');
        }
    }
    jobs
}

/// The apinata seed commit (`Repos::seed_commit`, the Potluck identity, message
/// `Seed apinata workspace`) on a server whose public host is `127.0.0.1`.
pub const SEED_COMMIT: &str = "1f7fbc63392e202f4f1eae88ba7e2509cd732d33";
