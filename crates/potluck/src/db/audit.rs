//! The append-only `audit` table: who did what to which target, written inside the
//! transaction of the change it records.
#![cfg_attr(not(test), allow(dead_code, reason = "wired by the audit wiring plan"))]

use std::net::IpAddr;

use super::DbError;
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
}
