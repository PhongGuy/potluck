//! The append-only `audit` table: who did what to which target, written inside the
//! transaction of the change it records.

use std::net::IpAddr;

use rusqlite::params_from_iter;
use rusqlite::types::Value;

use super::{Db, DbError};
use crate::clock;

/// Who acts and from where; built by the API from `Authenticated` and `ClientIp`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuditCtx {
    actor: AuditActor,
    client_ip: Option<IpAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuditActor {
    System,
    User { id: i64, name: String },
    Anonymous,
}

impl AuditCtx {
    /// The server itself, recorded as actor `system`.
    pub fn system() -> Self {
        Self {
            actor: AuditActor::System,
            client_ip: None,
        }
    }

    /// A signed-in user, by id and username.
    pub fn user(id: i64, username: &str) -> Self {
        Self {
            actor: AuditActor::User {
                id,
                name: username.to_owned(),
            },
            client_ip: None,
        }
    }

    /// A caller who has not signed in, such as a failed sign-in.
    pub(crate) fn anonymous() -> Self {
        Self {
            actor: AuditActor::Anonymous,
            client_ip: None,
        }
    }

    /// The same context with the client's address recorded.
    pub fn with_client_ip(self, ip: IpAddr) -> Self {
        Self {
            client_ip: Some(ip),
            ..self
        }
    }

    /// The `actor_kind`, `actor_id` and `actor_name` columns.
    fn actor_columns(&self) -> (&'static str, Option<i64>, Option<&str>) {
        match &self.actor {
            AuditActor::System => ("system", None, Some("system")),
            AuditActor::User { id, name } => ("user", Some(*id), Some(name)),
            AuditActor::Anonymous => ("anonymous", None, None),
        }
    }
}

/// One variant per audited action; `as_str` is the stored text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuditAction {
    SessionCreate,
    SessionFailed,
    TokenCreate,
    TokenRevoke,
    UserCreate,
    UserUpdate,
    TeamCreate,
    TeamMemberAdd,
    TeamMemberRemove,
    WorkspaceCreate,
    WorkspaceDelete,
    GrantPut,
    GrantDelete,
    OverrideArm,
    OverrideDisarm,
    OverrideUse,
}

impl AuditAction {
    /// The dotted name stored in the `action` column.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            AuditAction::SessionCreate => "session.create",
            AuditAction::SessionFailed => "session.failed",
            AuditAction::TokenCreate => "token.create",
            AuditAction::TokenRevoke => "token.revoke",
            AuditAction::UserCreate => "user.create",
            AuditAction::UserUpdate => "user.update",
            AuditAction::TeamCreate => "team.create",
            AuditAction::TeamMemberAdd => "team.member_add",
            AuditAction::TeamMemberRemove => "team.member_remove",
            AuditAction::WorkspaceCreate => "workspace.create",
            AuditAction::WorkspaceDelete => "workspace.delete",
            AuditAction::GrantPut => "grant.put",
            AuditAction::GrantDelete => "grant.delete",
            AuditAction::OverrideArm => "override.arm",
            AuditAction::OverrideDisarm => "override.disarm",
            AuditAction::OverrideUse => "override.use",
        }
    }
}

/// What a row says beyond the actor: target, workspace snapshot and typed detail.
pub(crate) struct Entry {
    action: AuditAction,
    target: Option<(&'static str, Option<i64>, Option<String>)>,
    workspace: Option<String>,
    detail: Option<String>,
}

impl Entry {
    /// An entry with only the action set.
    pub(crate) fn new(action: AuditAction) -> Self {
        Self {
            action,
            target: None,
            workspace: None,
            detail: None,
        }
    }

    /// The target's kind, and its id and name when known.
    pub(crate) fn target(self, kind: &'static str, id: Option<i64>, name: Option<&str>) -> Self {
        Self {
            target: Some((kind, id, name.map(str::to_owned))),
            ..self
        }
    }

    /// The workspace as `{team}/{ws}`, kept as text so the row outlives a rename or delete.
    pub(crate) fn workspace(self, team_ws: &str) -> Self {
        Self {
            workspace: Some(team_ws.to_owned()),
            ..self
        }
    }

    /// The detail as JSON; an encoding failure is `DbError::Sqlite`.
    pub(crate) fn detail<T: serde::Serialize>(self, detail: &T) -> Result<Self, DbError> {
        let json = serde_json::to_string(detail)
            .map_err(|e| DbError::Sqlite(format!("could not encode the audit detail: {e}")))?;
        Ok(Self {
            detail: Some(json),
            ..self
        })
    }
}

/// The `user.create` detail.
#[derive(serde::Serialize)]
pub(crate) struct UserCreated {
    pub(crate) server_admin: bool,
}

/// The `user.update` detail: the set fields in assignment order, the hash named `password`, and
/// the new value of each flag the patch set.
#[derive(serde::Serialize)]
pub(crate) struct UserUpdated {
    pub(crate) fields: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) server_admin: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) disabled: Option<bool>,
}

/// The `token.create` and `token.revoke` detail: the token owner's username.
#[derive(serde::Serialize)]
pub(crate) struct TokenOwner<'a> {
    pub(crate) user: &'a str,
}

/// The `team.member_add` and `team.member_remove` detail: the member's username.
#[derive(serde::Serialize)]
pub(crate) struct MemberDetail<'a> {
    pub(crate) user: &'a str,
}

/// The `workspace.create` detail: the kind and whether the repository was seeded.
#[derive(serde::Serialize)]
pub(crate) struct WorkspaceCreated<'a> {
    pub(crate) kind: &'a str,
    pub(crate) seed: bool,
}

/// The `grant.put` and `grant.delete` detail: the role before, null for a new grant, and the
/// role after, absent on a delete.
#[derive(serde::Serialize)]
pub(crate) struct GrantChange<'a> {
    pub(crate) old: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) new: Option<&'a str>,
}

/// The `override.use` detail: how many refs the consuming push moved.
#[derive(serde::Serialize)]
pub(crate) struct OverrideUsed {
    pub(crate) refs: usize,
}

/// Why a sign-in was refused; stored snake_case as the `session.failed` detail's `reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SignInFailure {
    /// No account has the given username.
    UnknownUser,
    /// The account is disabled.
    Disabled,
    /// The account has no password set.
    NoPassword,
    /// The password did not match.
    WrongPassword,
}

/// The `session.failed` detail.
#[derive(serde::Serialize)]
struct SignInFailed {
    reason: SignInFailure,
}

/// Inserts one row stamped now; call it on the open transaction of the change it records.
pub(crate) fn insert(
    conn: &rusqlite::Connection,
    ctx: &AuditCtx,
    entry: &Entry,
) -> rusqlite::Result<()> {
    let (actor_kind, actor_id, actor_name) = ctx.actor_columns();
    let (target_kind, target_id, target_name) = match &entry.target {
        Some((kind, id, name)) => (Some(*kind), *id, name.as_deref()),
        None => (None, None, None),
    };
    conn.execute(
        "INSERT INTO audit (at, action, actor_kind, actor_id, actor_name, target_kind, \
         target_id, target_name, workspace, client_ip, detail) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            clock::now_rfc3339(),
            entry.action.as_str(),
            actor_kind,
            actor_id,
            actor_name,
            target_kind,
            target_id,
            target_name,
            entry.workspace,
            ctx.client_ip.map(|ip| ip.to_string()),
            entry.detail,
        ],
    )?;
    Ok(())
}

/// Which rows `Db::audit` returns: each set field narrows the result, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuditFilter {
    actor: Option<String>,
    workspace: Option<String>,
    action: Option<String>,
    since: Option<String>,
    until: Option<String>,
    before: Option<i64>,
    limit: u32,
}

impl AuditFilter {
    /// No filters and a limit of 100.
    pub fn new() -> Self {
        Self {
            actor: None,
            workspace: None,
            action: None,
            since: None,
            until: None,
            before: None,
            limit: 100,
        }
    }

    /// Rows whose `actor_name` is `name`.
    pub fn actor(self, name: &str) -> Self {
        Self {
            actor: Some(name.to_owned()),
            ..self
        }
    }

    /// Rows about the workspace `{team}/{ws}`.
    pub fn workspace(self, team_ws: &str) -> Self {
        Self {
            workspace: Some(team_ws.to_owned()),
            ..self
        }
    }

    /// Rows with this dotted action name.
    pub fn action(self, action: &str) -> Self {
        Self {
            action: Some(action.to_owned()),
            ..self
        }
    }

    /// Rows stamped at or after `stamp`, which the caller has normalised to the stored UTC form.
    pub fn since(self, stamp: &str) -> Self {
        Self {
            since: Some(stamp.to_owned()),
            ..self
        }
    }

    /// Rows stamped before `stamp`, in the stored UTC form.
    pub fn until(self, stamp: &str) -> Self {
        Self {
            until: Some(stamp.to_owned()),
            ..self
        }
    }

    /// Rows with an id below `id`: the keyset cursor for the next page.
    pub fn before(self, id: i64) -> Self {
        Self {
            before: Some(id),
            ..self
        }
    }

    /// At most `n` rows; the caller keeps it in `1..=1000`.
    pub fn limit(self, n: u32) -> Self {
        Self { limit: n, ..self }
    }

    /// The `WHERE` terms and their values. Column names are fixed text here, never caller input.
    fn conditions(self) -> Vec<(&'static str, Value)> {
        let text = |column, v: Option<String>| v.map(|v| (column, Value::Text(v)));
        [
            text("actor_name = ?", self.actor),
            text("workspace = ?", self.workspace),
            text("action = ?", self.action),
            text("at >= ?", self.since),
            text("at < ?", self.until),
            self.before.map(|id| ("id < ?", Value::Integer(id))),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

impl Default for AuditFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// One audit row as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuditRecord {
    id: i64,
    at: String,
    action: String,
    actor_kind: String,
    actor_id: Option<i64>,
    actor_name: Option<String>,
    target_kind: Option<String>,
    target_id: Option<i64>,
    target_name: Option<String>,
    workspace: Option<String>,
    client_ip: Option<String>,
    detail: Option<String>,
}

impl AuditRecord {
    /// The row id, increasing with insertion order.
    pub fn id(&self) -> i64 {
        self.id
    }
    /// The UTC stamp, `YYYY-MM-DDTHH:MM:SSZ`.
    pub fn at(&self) -> &str {
        &self.at
    }
    /// The dotted action name, possibly one this build does not know.
    pub fn action(&self) -> &str {
        &self.action
    }
    /// `user`, `system` or `anonymous`.
    pub fn actor_kind(&self) -> &str {
        &self.actor_kind
    }
    /// The acting user's id.
    pub fn actor_id(&self) -> Option<i64> {
        self.actor_id
    }
    /// The acting user's name at the time, or `system`.
    pub fn actor_name(&self) -> Option<&str> {
        self.actor_name.as_deref()
    }
    /// The kind of thing acted on.
    pub fn target_kind(&self) -> Option<&str> {
        self.target_kind.as_deref()
    }
    /// The target's id.
    pub fn target_id(&self) -> Option<i64> {
        self.target_id
    }
    /// The target's name at the time.
    pub fn target_name(&self) -> Option<&str> {
        self.target_name.as_deref()
    }
    /// The workspace as `{team}/{ws}`.
    pub fn workspace(&self) -> Option<&str> {
        self.workspace.as_deref()
    }
    /// The client address the request came from.
    pub fn client_ip(&self) -> Option<&str> {
        self.client_ip.as_deref()
    }
    /// The detail as stored JSON text.
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

/// Which retention a row falls under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditClass {
    /// Every action but `session.failed`.
    Changes,
    /// `session.failed`.
    SignInFailures,
}

fn record_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AuditRecord> {
    Ok(AuditRecord {
        id: r.get(0)?,
        at: r.get(1)?,
        action: r.get(2)?,
        actor_kind: r.get(3)?,
        actor_id: r.get(4)?,
        actor_name: r.get(5)?,
        target_kind: r.get(6)?,
        target_id: r.get(7)?,
        target_name: r.get(8)?,
        workspace: r.get(9)?,
        client_ip: r.get(10)?,
        detail: r.get(11)?,
    })
}

impl Db {
    /// The rows matching `filter`, newest first, at most `filter.limit` of them.
    pub async fn audit(&self, filter: AuditFilter) -> Result<Vec<AuditRecord>, DbError> {
        self.call(move |conn| {
            // Each set filter adds one bound term; the limit is bound last.
            let limit = filter.limit;
            let (terms, mut values): (Vec<_>, Vec<_>) = filter.conditions().into_iter().unzip();
            let clause = if terms.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", terms.join(" AND "))
            };
            values.push(Value::Integer(i64::from(limit)));
            let sql = format!(
                "SELECT id, at, action, actor_kind, actor_id, actor_name, target_kind, target_id, \
                 target_name, workspace, client_ip, detail FROM audit{clause} ORDER BY id DESC LIMIT ?"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(values), record_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// One `session.failed` row: actor `anonymous`, target the user only when `user` is given,
    /// so a name that matched no account is never stored.
    pub async fn record_failed_sign_in(
        &self,
        user: Option<(i64, &str)>,
        reason: SignInFailure,
        client_ip: Option<IpAddr>,
    ) -> Result<(), DbError> {
        let user = user.map(|(id, name)| (id, name.to_owned()));
        let ctx = match client_ip {
            Some(ip) => AuditCtx::anonymous().with_client_ip(ip),
            None => AuditCtx::anonymous(),
        };
        self.call(move |conn| {
            let mut entry = Entry::new(AuditAction::SessionFailed);
            if let Some((id, name)) = &user {
                entry = entry.target("user", Some(*id), Some(name));
            }
            insert(conn, &ctx, &entry.detail(&SignInFailed { reason })?)?;
            Ok(())
        })
        .await
    }

    /// Deletes at most `limit` rows of `class` with `at < before`; returns how many went.
    pub async fn prune_audit(
        &self,
        class: AuditClass,
        before: &str,
        limit: u32,
    ) -> Result<usize, DbError> {
        let before = before.to_owned();
        let failures = class == AuditClass::SignInFailures;
        self.call(move |conn| {
            // The class test keeps a flood of failed sign-ins from ever taking change rows.
            let gone = conn.execute(
                "DELETE FROM audit WHERE id IN (SELECT id FROM audit WHERE at < ?1 \
                 AND (action = ?2) = ?3 ORDER BY id LIMIT ?4)",
                rusqlite::params![before, AuditAction::SessionFailed.as_str(), failures, limit],
            )?;
            Ok(gone)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn conn() -> rusqlite::Connection {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        super::super::migrations::migrate_with(&mut c, super::super::migrations::MIGRATIONS)
            .unwrap();
        c
    }

    type Row = (
        String,
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );

    fn rows(c: &rusqlite::Connection) -> Vec<Row> {
        let mut stmt = c
            .prepare(
                "SELECT action, actor_kind, actor_id, actor_name, target_kind, target_id, \
                 target_name, workspace, client_ip, detail FROM audit ORDER BY id",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    #[test]
    fn every_action_has_its_dotted_name() {
        let names = [
            (AuditAction::SessionCreate, "session.create"),
            (AuditAction::SessionFailed, "session.failed"),
            (AuditAction::TokenCreate, "token.create"),
            (AuditAction::TokenRevoke, "token.revoke"),
            (AuditAction::UserCreate, "user.create"),
            (AuditAction::UserUpdate, "user.update"),
            (AuditAction::TeamCreate, "team.create"),
            (AuditAction::TeamMemberAdd, "team.member_add"),
            (AuditAction::TeamMemberRemove, "team.member_remove"),
            (AuditAction::WorkspaceCreate, "workspace.create"),
            (AuditAction::WorkspaceDelete, "workspace.delete"),
            (AuditAction::GrantPut, "grant.put"),
            (AuditAction::GrantDelete, "grant.delete"),
            (AuditAction::OverrideArm, "override.arm"),
            (AuditAction::OverrideDisarm, "override.disarm"),
            (AuditAction::OverrideUse, "override.use"),
        ];
        for (action, name) in names {
            assert_eq!(action.as_str(), name);
        }
    }

    #[test]
    fn a_user_row_carries_actor_target_workspace_ip_and_detail() {
        let c = conn();
        let ctx = AuditCtx::user(7, "bob").with_client_ip(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9)));
        let entry = Entry::new(AuditAction::GrantPut)
            .target("user", Some(8), Some("ann"))
            .workspace("core/api")
            .detail(&serde_json::json!({"old": null, "new": "write"}))
            .unwrap();
        insert(&c, &ctx, &entry).unwrap();
        assert_eq!(
            rows(&c),
            [(
                "grant.put".to_owned(),
                "user".to_owned(),
                Some(7),
                Some("bob".to_owned()),
                Some("user".to_owned()),
                Some(8),
                Some("ann".to_owned()),
                Some("core/api".to_owned()),
                Some("192.0.2.9".to_owned()),
                Some(r#"{"new":"write","old":null}"#.to_owned()),
            )]
        );
    }

    #[test]
    fn system_and_anonymous_rows_have_no_actor_id_and_no_ip_by_default() {
        let c = conn();
        insert(
            &c,
            &AuditCtx::system(),
            &Entry::new(AuditAction::TeamCreate),
        )
        .unwrap();
        insert(
            &c,
            &AuditCtx::anonymous(),
            &Entry::new(AuditAction::SessionFailed),
        )
        .unwrap();
        let got = rows(&c);
        assert_eq!(
            (got[0].1.as_str(), got[0].2, got[0].3.as_deref()),
            ("system", None, Some("system"))
        );
        assert_eq!(
            (got[1].1.as_str(), got[1].2, got[1].3.as_deref()),
            ("anonymous", None, None)
        );
        assert!(
            got.iter()
                .all(|r| r.4.is_none() && r.7.is_none() && r.8.is_none() && r.9.is_none())
        );
    }

    #[test]
    fn the_stamp_is_the_twenty_character_utc_form() {
        let c = conn();
        insert(
            &c,
            &AuditCtx::system(),
            &Entry::new(AuditAction::TeamCreate),
        )
        .unwrap();
        let at: String = c
            .query_row("SELECT at FROM audit", [], |r| r.get(0))
            .unwrap();
        assert_eq!(at.len(), 20, "{at}");
        assert!(at.ends_with('Z'), "{at}");
    }

    async fn db_with_250() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("server.db")).unwrap();
        db.migrate().await.unwrap();
        db.call(|c| {
            // Row n: minute n of 2026-03-01, actor bob for even n, ann for odd; workspace core/api
            // for n % 3 == 0; action team.create for n % 5 == 0, else grant.put.
            let tx = c.transaction()?;
            for n in 0..250i64 {
                let at = format!("2026-03-01T{:02}:{:02}:00Z", n / 60, n % 60);
                let actor = if n % 2 == 0 { "bob" } else { "ann" };
                let ws = (n % 3 == 0).then_some("core/api");
                let action = if n % 5 == 0 {
                    "team.create"
                } else {
                    "grant.put"
                };
                tx.execute(
                    "INSERT INTO audit (at, action, actor_kind, actor_name, workspace) \
                     VALUES (?1, ?2, 'user', ?3, ?4)",
                    rusqlite::params![at, action, actor, ws],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn pages_are_newest_first_disjoint_and_complete() {
        let (_dir, db) = db_with_250().await;
        let mut seen = Vec::new();
        let mut filter = AuditFilter::new().limit(100);
        loop {
            let page = db.audit(filter.clone()).await.unwrap();
            assert!(
                page.windows(2).all(|w| w[0].id() > w[1].id()),
                "newest first"
            );
            let Some(last) = page.last() else { break };
            seen.extend(page.iter().map(AuditRecord::id));
            filter = filter.before(last.id());
        }
        assert_eq!(seen.len(), 250);
        assert!(
            seen.windows(2).all(|w| w[0] > w[1]),
            "strictly descending across pages, so no row twice"
        );
    }

    #[tokio::test]
    async fn the_default_limit_is_100() {
        let (_dir, db) = db_with_250().await;
        assert_eq!(db.audit(AuditFilter::new()).await.unwrap().len(), 100);
    }

    #[tokio::test]
    async fn filters_combine() {
        let (_dir, db) = db_with_250().await;
        let all = AuditFilter::new().limit(1000);
        let bob = db.audit(all.clone().actor("bob")).await.unwrap();
        assert_eq!(bob.len(), 125);
        assert!(bob.iter().all(|r| r.actor_name() == Some("bob")));
        let ws = db.audit(all.clone().workspace("core/api")).await.unwrap();
        assert_eq!(ws.len(), 84);
        let created = db.audit(all.clone().action("team.create")).await.unwrap();
        assert_eq!(created.len(), 50);
        // Minutes 60..=119 are hour 01.
        let hour = db
            .audit(
                all.clone()
                    .since("2026-03-01T01:00:00Z")
                    .until("2026-03-01T02:00:00Z"),
            )
            .await
            .unwrap();
        assert_eq!(hour.len(), 60);
        assert!(hour.iter().all(|r| r.at().starts_with("2026-03-01T01:")));
        let both = db
            .audit(all.actor("bob").action("team.create"))
            .await
            .unwrap();
        assert_eq!(both.len(), 25, "even and a multiple of five");
        assert!(
            db.audit(AuditFilter::new().actor("nobody"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_unknown_action_reads_back_unchanged() {
        let (_dir, db) = db_with_250().await;
        db.call(|c| {
            c.execute(
                "INSERT INTO audit (at, action, actor_kind, detail) \
                 VALUES ('2026-04-01T00:00:00Z', 'oidc.link', 'system', '{\"k\":1}')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let top = db.audit(AuditFilter::new().limit(1)).await.unwrap();
        assert_eq!(top[0].action(), "oidc.link");
        assert_eq!(top[0].detail(), Some("{\"k\":1}"));
    }

    async fn seed(db: &Db, rows: &[(&'static str, &'static str)]) {
        let rows = rows.to_vec();
        db.call(move |c| {
            for (at, action) in rows {
                c.execute(
                    "INSERT INTO audit (at, action, actor_kind) VALUES (?1, ?2, 'anonymous')",
                    [at, action],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    }

    async fn left(db: &Db) -> Vec<(String, String)> {
        db.audit(AuditFilter::new().limit(1000))
            .await
            .unwrap()
            .into_iter()
            .rev()
            .map(|r| (r.at().to_owned(), r.action().to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn each_class_prunes_only_its_own_old_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("server.db")).unwrap();
        db.migrate().await.unwrap();
        seed(
            &db,
            &[
                ("2026-01-01T00:00:00Z", "user.create"),
                ("2026-01-01T00:00:00Z", "session.failed"),
                ("2026-06-01T00:00:00Z", "user.create"),
                ("2026-06-01T00:00:00Z", "session.failed"),
            ],
        )
        .await;
        let cut = "2026-03-01T00:00:00Z";
        assert_eq!(
            db.prune_audit(AuditClass::SignInFailures, cut, 5000)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            db.prune_audit(AuditClass::Changes, cut, 5000)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            left(&db).await,
            [
                ("2026-06-01T00:00:00Z".to_owned(), "user.create".to_owned()),
                (
                    "2026-06-01T00:00:00Z".to_owned(),
                    "session.failed".to_owned()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_failure_flood_never_takes_change_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("server.db")).unwrap();
        db.migrate().await.unwrap();
        seed(&db, &[("2026-01-01T00:00:00Z", "grant.put")]).await;
        let flood: Vec<_> = (0..300)
            .map(|_| ("2026-01-01T00:00:00Z", "session.failed"))
            .collect();
        seed(&db, &flood).await;
        let mut gone = 0;
        loop {
            let n = db
                .prune_audit(AuditClass::SignInFailures, "2026-02-01T00:00:00Z", 100)
                .await
                .unwrap();
            assert!(n <= 100, "a batch is capped");
            gone += n;
            if n < 100 {
                break;
            }
        }
        assert_eq!(gone, 300);
        assert_eq!(
            left(&db).await,
            [("2026-01-01T00:00:00Z".to_owned(), "grant.put".to_owned())]
        );
    }
}
