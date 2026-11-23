//! Shared helpers. Every test reaches only 127.0.0.1 or a Unix socket under a temp dir.
pub mod idp;
pub mod release;
pub mod scratch;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use clap::Parser;
use potluck::admin::AdminError;
use potluck::api::AppState;
use potluck::api::credential::Credential;
use potluck::api::error::ApiError;
use potluck::auth::password::Passwords;
use potluck::auth::token::MintedToken;
use potluck::cli::{Cli, Command as CliCommand};
use potluck::config::{Config, ConfigError, ServeArgs};
use potluck::db::Db;
use potluck::db::accounts::{NewToken, NewUser, UserRecord};
use potluck::git::access::{Access, BoxFuture, GitAccess, PushContext};
use potluck::git::{GitLimits, Repos};
use potluck::notices::Notices;
use potluck::role::Role;
use potluck::server::{Running, start, start_with};
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
    config_listening_on(dir, "127.0.0.1:0".parse().expect("loopback"))
}

/// A `Config` for a server on `listen` with `dir` as its data directory.
pub fn config_listening_on(dir: &Path, listen: SocketAddr) -> Config {
    let data = dir.to_str().expect("utf-8 temp path");
    let listen = listen.to_string();
    parse_config(&[
        "--listen",
        &listen,
        "--data",
        data,
        "--public-url",
        "http://127.0.0.1:8080",
    ])
    .expect("test config")
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

/// A started server with the git routes over a temp data dir and a `TestAccess`. The data dir
/// is `dir`, so the hooks dir is `dir.path().join("hooks")`. No field without a reader: this is
/// a test binary, where an unread field warns even when `pub`.
pub struct GitServer {
    pub dir: tempfile::TempDir,
    pub base: String,
    pub repos: Repos,
    pub db: Db,
    pub notices: Notices,
    _running: Running,
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
    std::fs::create_dir_all(config.data()).expect("data dir");
    let db = Db::open(&config.db_path()).expect("open");
    let repos = Repos::new(config.repos_dir(), config.git().to_path_buf());
    let mut state = AppState::new(db.clone(), config.clone()).with_git_access(Arc::new(access));
    if let Some(limits) = limits {
        state = state.with_git_limits(limits);
    }
    let notices = state.notices().clone();
    let running = start_with(&config, state).await.expect("start");
    install_binary_hook(&config);
    GitServer {
        dir,
        base: format!("http://{}", running.http_addr()),
        repos,
        db,
        notices,
        _running: running,
    }
}

/// Replaces the hook `start_with` installed, which execs this test binary, with one that runs
/// the built `potluck` binary's secret check.
fn install_binary_hook(config: &Config) {
    write_script(
        &config.hooks_dir().join("pre-receive"),
        &potluck::hooks::script(Path::new(env!("CARGO_BIN_EXE_potluck")), config.git())
            .expect("hook script"),
    );
}

/// `server` served again on a new port with `access` as its resolver; the data dir, database
/// and repositories are the same. The old server shuts down first, releasing the data dir lock
/// and closing its database, which is opened again here.
pub async fn git_server_with(server: GitServer, access: TestAccess) -> GitServer {
    let GitServer {
        dir,
        repos,
        _running: old,
        ..
    } = server;
    old.shutdown().await;
    // The data dir, repositories and migrated database carry over; the server and resolver are new.
    let data = dir.path().to_str().expect("utf-8 temp path").to_owned();
    let config = parse_config(&[
        "--listen",
        "127.0.0.1:0",
        "--data",
        &data,
        "--public-url",
        "http://127.0.0.1:8080",
    ])
    .expect("test config");
    let db = Db::open(&config.db_path()).expect("open");
    let state = AppState::new(db.clone(), config.clone()).with_git_access(Arc::new(access));
    let notices = state.notices().clone();
    let running = start_with(&config, state).await.expect("start");
    install_binary_hook(&config);
    GitServer {
        dir,
        base: format!("http://{}", running.http_addr()),
        repos,
        db,
        notices,
        _running: running,
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

/// `text` at `path`, mode 0755, written by a child `sh` to a temporary name and renamed into
/// place, so this process never holds a write fd a sibling thread's fork could inherit and turn
/// the exec into ETXTBSY. Panics with the path if any step fails.
pub fn write_script(path: &Path, text: &str) {
    use std::io::Write;
    let mut child = Command::new("sh")
        .args([
            "-c",
            r#"cat > "$1.tmp" && chmod 755 "$1.tmp" && mv -f "$1.tmp" "$1""#,
            "sh",
        ])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    // Close stdin and reap the child before judging either step, so no failure strands it.
    let written = child.stdin.take().unwrap().write_all(text.as_bytes());
    let status = child.wait();
    match (written, status) {
        (Ok(()), Ok(status)) if status.success() => {}
        outcome => panic!("writing {}: {outcome:?}", path.display()),
    }
}

/// `<dir>/pre-receive` as `#!/bin/sh` plus `body`, mode 0755, over whatever is there.
pub fn write_hook(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir).expect("hooks dir");
    write_script(&dir.join("pre-receive"), &format!("#!/bin/sh\n{body}\n"));
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

/// A real server on a fresh temp data dir, as `full_server` starts it; the dir keeps it alive.
/// Hashing runs at the production argon2 cost, so create as few users as a test needs.
pub async fn admin_server() -> (tempfile::TempDir, Running) {
    let FullServer { dir, running } = full_server().await;
    (dir, running)
}

/// `potluck admin --data <data> <args..>` through the real command tree, run in process with
/// `stdin` as its input; returns the result and what it wrote to stdout.
pub async fn admin(data: &Path, args: &[&str], stdin: &str) -> (Result<(), AdminError>, String) {
    let data = data.to_str().expect("utf-8 temp path");
    let argv = ["potluck", "admin", "--data", data]
        .into_iter()
        .chain(args.iter().copied());
    let cli = Cli::try_parse_from(argv).expect("admin flags parse");
    let CliCommand::Admin(args) = cli.command else {
        panic!("not the admin command")
    };
    let mut input = std::io::Cursor::new(stdin.as_bytes().to_vec());
    let mut out = Vec::new();
    let result = potluck::admin::run(*args, &mut input, &mut out).await;
    (result, String::from_utf8(out).expect("utf-8 output"))
}

/// A table line's cells: split on the two-space gutter, so a header reads the same however
/// wide its columns were padded.
pub fn columns(line: &str) -> Vec<&str> {
    line.split("  ")
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect()
}

/// A started server over a migrated database with fast hashing; the dir and the server live as
/// long as it.
pub struct TestApp {
    pub _dir: tempfile::TempDir,
    pub db: Db,
    pub addr: SocketAddr,
    pub _running: Running,
}

impl TestApp {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}/api/v1{path}", self.addr)
    }
}

pub async fn test_app() -> TestApp {
    let (dir, db, state) = test_state().await;
    let running = start_with(&config_for(dir.path()), state)
        .await
        .expect("start");
    TestApp {
        _dir: dir,
        db,
        addr: running.http_addr(),
        _running: running,
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

/// An `opencollection.yml` in a format the apinata kind lists.
pub const APINATA_FORMAT: &[u8] = b"opencollection: 1.0.0\n";

/// The apinata seed commit (`Repos::seed_commit`, the Potluck identity, message
/// `Seed apinata workspace`) on a server whose public host is `127.0.0.1`.
pub const SEED_COMMIT: &str = "1f7fbc63392e202f4f1eae88ba7e2509cd732d33";

/// A reader over an open SSE response, block by block, through `Response::chunk`.
pub struct SseReader {
    response: reqwest::Response,
    buf: Vec<u8>,
}

impl SseReader {
    /// `GET url` with `token`; the reader on 200, else the status.
    pub async fn open(url: &str, token: &str) -> Result<SseReader, u16> {
        let r = client()
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .expect("request");
        if r.status().as_u16() != 200 {
            return Err(r.status().as_u16());
        }
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        Ok(SseReader {
            response: r,
            buf: Vec::new(),
        })
    }

    /// The lines of the next block that is not a comment-only keep-alive, with LF or CRLF line
    /// ends; `None` once the server closed the stream with only blank or comment lines left.
    /// Panics with `stream ended mid-event` when the stream ends inside an event, since a client
    /// discards such an event.
    pub async fn next_block(&mut self) -> Option<Vec<String>> {
        loop {
            if let Some(end) = self.buf.windows(2).position(|w| w == b"\n\n") {
                let block: Vec<u8> = self.buf.drain(..end + 2).collect();
                let lines = event_lines(&block);
                if lines.is_empty() {
                    continue;
                }
                return Some(lines);
            }
            match self.response.chunk().await.expect("read") {
                // CRLF is folded to LF over the whole buffer, not per chunk, so a pair split
                // across two chunks joins up first and counts as one line end.
                Some(bytes) => {
                    self.buf.extend_from_slice(&bytes);
                    let buf = std::mem::take(&mut self.buf);
                    self.buf = buf
                        .iter()
                        .enumerate()
                        .filter(|&(i, &b)| b != b'\r' || buf.get(i + 1) != Some(&b'\n'))
                        .map(|(_, &b)| b)
                        .collect();
                }
                None => {
                    let leftover = event_lines(&self.buf);
                    assert!(leftover.is_empty(), "stream ended mid-event: {leftover:?}");
                    return None;
                }
            }
        }
    }
}

/// The non-empty, non-comment lines of an SSE block.
fn event_lines(block: &[u8]) -> Vec<String> {
    std::str::from_utf8(block)
        .expect("utf-8")
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with(':'))
        .map(str::to_owned)
        .collect()
}

/// `(ref, sha)` of a `ref` event block.
pub fn ref_event(block: &[String]) -> (String, String) {
    assert_eq!(block[0], "event: ref", "{block:?}");
    let data = block[1].strip_prefix("data: ").expect("data line");
    let v: serde_json::Value = serde_json::from_str(data).expect("json");
    (
        v["ref"].as_str().unwrap().to_owned(),
        v["sha"].as_str().unwrap().to_owned(),
    )
}

/// `POST /session` over TCP: `Ok(token id)` on 201, else `Err(status)`.
pub async fn session(addr: SocketAddr, username: &str, password: &str) -> Result<i64, u16> {
    let r = client()
        .post(format!("http://{addr}/api/v1/session"))
        .json(&serde_json::json!({"username": username, "password": password, "token_name": "cli-test"}))
        .send()
        .await
        .expect("session request");
    let status = r.status().as_u16();
    if status != 201 {
        return Err(status);
    }
    let body: serde_json::Value = r.json().await.expect("json");
    Ok(body["token_meta"]["id"].as_i64().expect("token id"))
}

/// A git wrapper at `<dir>/hanging-git` that hangs on every call with an argument exactly `step`
/// (such as `init` or `hash-object`), from the first on, and otherwise runs the real git from `PATH`.
pub fn hanging_git(dir: &Path, step: &str) -> std::path::PathBuf {
    hanging_git_on_call(dir, step, 1)
}

/// Like `hanging_git`, but each `step` call is counted in `<dir>/hung.calls` and the hang starts
/// from the `nth` one. The hang starts a child in the same process group, records its pid in
/// `<dir>/hung-child.pid` and then its own in `<dir>/hung.pid`, and lasts while `<dir>/hold`
/// exists, at most 60 s, so removing the temp dir ends it even when nothing kills it.
pub fn hanging_git_on_call(dir: &Path, step: &str, nth: usize) -> std::path::PathBuf {
    let wrapper = dir.join("hanging-git");
    let hold = dir.join("hold");
    std::fs::write(&hold, b"").expect("hold file");
    write_script(
        &wrapper,
        &format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = '{step}' ]; then\n    echo >> '{calls}'\n    if [ $(($(wc -l < '{calls}'))) -ge {nth} ]; then\n      (n=0; while [ -e '{hold}' ] && [ $n -lt 600 ]; do sleep 0.1; n=$((n+1)); done) &\n      echo $! > '{child}'\n      echo $$ > '{pid}'\n      wait\n      exit 1\n    fi\n    break\n  fi\ndone\nexec git \"$@\"\n",
            calls = dir.join("hung.calls").display(),
            child = dir.join("hung-child.pid").display(),
            pid = dir.join("hung.pid").display(),
            hold = hold.display(),
        ),
    );
    wrapper
}

/// Whether the hung git is gone: its leader (`<dir>/hung.pid`) must already be reaped, and the
/// group child (`<dir>/hung-child.pid`) must be dead within 2 s. A zombie child counts as dead,
/// since its reaper is whoever adopted it, not the code under test.
pub fn hung_git_is_gone(dir: &Path) -> bool {
    let read = |name: &str| {
        let pid = std::fs::read_to_string(dir.join(name)).expect("the wrapper hung");
        pid.trim().to_owned()
    };
    if Path::new(&format!("/proc/{}", read("hung.pid"))).exists() {
        return false;
    }
    let stat = format!("/proc/{}/stat", read("hung-child.pid"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        // The state field follows the parenthesised command name.
        let dead = std::fs::read_to_string(&stat).map_or(true, |s| {
            s.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state == "Z" || state == "X")
        });
        if dead {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
