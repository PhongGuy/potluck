//! `potluck admin grant ...` and `override ...`: who may use a workspace, and who may push past
//! the secret check once.

use serde_json::{Value, json};

use super::client::Verb;
use super::teams::WorkspaceRef;
use super::{AdminError, Context, output, segment};

/// Who a grant is for.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubjectKind {
    /// One account.
    User,
    /// Every member of a team.
    Team,
}

impl SubjectKind {
    fn as_str(self) -> &'static str {
        match self {
            SubjectKind::User => "user",
            SubjectKind::Team => "team",
        }
    }
}

/// A workspace role, as the CLI spells it; kept apart from `Role` so the core type stays
/// free of clap.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RoleArg {
    /// Fetch and clone.
    Read,
    /// Push as well.
    Write,
    /// Manage grants and overrides as well.
    Admin,
}

impl RoleArg {
    fn as_str(self) -> &'static str {
        match self {
            RoleArg::Read => "read",
            RoleArg::Write => "write",
            RoleArg::Admin => "admin",
        }
    }
}

/// The `grant` verbs.
#[derive(clap::Subcommand, Debug)]
#[non_exhaustive]
pub enum GrantCmd {
    /// The grants on a workspace.
    List {
        /// As `{team}/{ws}`.
        workspace: WorkspaceRef,
    },
    /// Give a user or team a role on a workspace (replacing any earlier grant).
    Set {
        /// As `{team}/{ws}`.
        workspace: WorkspaceRef,
        /// Whether `name` is a user or a team.
        kind: SubjectKind,
        /// The username or team slug.
        name: String,
        /// The role to grant.
        role: RoleArg,
    },
    /// Remove a user's or team's grant.
    Remove {
        /// As `{team}/{ws}`.
        workspace: WorkspaceRef,
        /// Whether `name` is a user or a team.
        kind: SubjectKind,
        /// The username or team slug.
        name: String,
    },
}

/// The `override` verbs.
#[derive(clap::Subcommand, Debug)]
#[non_exhaustive]
pub enum OverrideCmd {
    /// The overrides on a workspace, oldest first.
    List {
        /// As `{team}/{ws}`.
        workspace: WorkspaceRef,
    },
    /// Let one user's next push skip the secret check, with a reason that is kept.
    Arm {
        /// As `{team}/{ws}`.
        workspace: WorkspaceRef,
        /// The user whose next push may skip the check.
        #[arg(long)]
        for_user: String,
        /// Why, kept with the override.
        #[arg(long)]
        reason: String,
    },
    /// Disarm an override that has not been consumed.
    Disarm {
        /// As `{team}/{ws}`.
        workspace: WorkspaceRef,
        /// The override's id.
        id: i64,
    },
}

pub(crate) async fn run_grant(cmd: GrantCmd, cx: &mut Context<'_>) -> Result<(), AdminError> {
    match cmd {
        GrantCmd::List { workspace } => {
            let target = format!("/api/v1/workspaces/{workspace}/grants");
            cx.call_list(Verb::Get, &target, None, &[200], output::grants)
                .await
        }
        GrantCmd::Set {
            workspace,
            kind,
            name,
            role,
        } => {
            let target = grant_path(&workspace, kind, &name)?;
            let body = json!({"role": role.as_str()}).to_string();
            cx.call(Verb::Put, &target, Some(&body), &[200], |v| {
                output::grants(std::slice::from_ref(v))
            })
            .await
        }
        GrantCmd::Remove {
            workspace,
            kind,
            name,
        } => {
            let target = grant_path(&workspace, kind, &name)?;
            let line = format!("removed grant {}:{name} from {workspace}\n", kind.as_str());
            cx.confirm(Verb::Delete, &target, &line).await
        }
    }
}

pub(crate) async fn run_override(cmd: OverrideCmd, cx: &mut Context<'_>) -> Result<(), AdminError> {
    match cmd {
        OverrideCmd::List { workspace } => {
            let target = format!("/api/v1/workspaces/{workspace}/overrides");
            cx.call_list(Verb::Get, &target, None, &[200], output::overrides)
                .await
        }
        OverrideCmd::Arm {
            workspace,
            for_user,
            reason,
        } => {
            // Both go in the body, and the server validates them, so neither is checked here.
            let target = format!("/api/v1/workspaces/{workspace}/overrides");
            let body = json!({"for_user": for_user, "reason": reason}).to_string();
            cx.call(Verb::Post, &target, Some(&body), &[201], one_override)
                .await
        }
        OverrideCmd::Disarm { workspace, id } => {
            let target = format!("/api/v1/workspaces/{workspace}/overrides/{id}");
            cx.confirm(Verb::Delete, &target, &format!("disarmed override {id}\n"))
                .await
        }
    }
}

fn grant_path(
    workspace: &WorkspaceRef,
    kind: SubjectKind,
    name: &str,
) -> Result<String, AdminError> {
    let name = segment("name", name)?;
    Ok(format!(
        "/api/v1/workspaces/{workspace}/grants/{}/{name}",
        kind.as_str()
    ))
}

fn one_override(record: &Value) -> String {
    output::overrides(std::slice::from_ref(record))
}
