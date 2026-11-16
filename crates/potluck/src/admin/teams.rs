//! `potluck admin team ...` and `workspace ...`: teams, their members and their workspaces.

use std::fmt;
use std::str::FromStr;

use serde_json::{Value, json};

use super::client::Verb;
use super::{AdminError, Context, output, segment};

/// `{team}/{ws}` as one argument: exactly one slash, both parts valid names.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WorkspaceRef {
    team: String,
    ws: String,
}

impl FromStr for WorkspaceRef {
    type Err = String;

    /// Both halves go into the request path, so each must pass `segment` at parse time.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (team, ws) = s
            .split_once('/')
            .filter(|(team, ws)| !team.is_empty() && !ws.is_empty() && !ws.contains('/'))
            .ok_or_else(|| "expected {team}/{ws}".to_owned())?;
        Ok(WorkspaceRef {
            team: segment("team", team)
                .map_err(|e| e.to_string())?
                .to_string(),
            ws: segment("workspace", ws)
                .map_err(|e| e.to_string())?
                .to_string(),
        })
    }
}

impl fmt::Display for WorkspaceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.team, self.ws)
    }
}

/// The `team` verbs.
#[derive(clap::Subcommand, Debug)]
#[non_exhaustive]
pub enum TeamCmd {
    /// Every team with its members.
    List,
    /// Create a team.
    Create {
        /// The team's name in URLs.
        slug: String,
        /// Defaults to the slug.
        #[arg(long)]
        name: Option<String>,
    },
    /// Add a user to a team.
    AddMember {
        /// The team's slug.
        team: String,
        /// The user to add.
        username: String,
    },
    /// Remove a user from a team.
    RemoveMember {
        /// The team's slug.
        team: String,
        /// The user to remove.
        username: String,
    },
}

/// The `workspace` verbs.
#[derive(clap::Subcommand, Debug)]
#[non_exhaustive]
pub enum WorkspaceCmd {
    /// Every workspace, optionally of one kind.
    List {
        /// Only workspaces of this app kind.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Create a workspace in a team.
    Create {
        /// The owning team's slug.
        team: String,
        /// The workspace's name in URLs.
        slug: String,
        /// The app kind, such as apinata.
        #[arg(long)]
        kind: String,
        /// Defaults to the slug.
        #[arg(long)]
        name: Option<String>,
        /// The branch clients push to.
        #[arg(long, default_value = "main")]
        default_branch: String,
        /// Leave the repository empty for an import instead of seeding it.
        #[arg(long)]
        import: bool,
    },
    /// One workspace.
    Show {
        /// As `{team}/{ws}`.
        workspace: WorkspaceRef,
    },
    /// Delete a workspace and its repository.
    Delete {
        /// As `{team}/{ws}`.
        workspace: WorkspaceRef,
    },
}

pub(crate) async fn run_team(cmd: TeamCmd, cx: &mut Context<'_>) -> Result<(), AdminError> {
    match cmd {
        TeamCmd::List => {
            cx.call_list(Verb::Get, "/api/v1/teams", None, &[200], output::teams)
                .await
        }
        TeamCmd::Create { slug, name } => {
            // The server requires a name; the slug stands in when none is given.
            let body = json!({"name": name.as_deref().unwrap_or(&slug), "slug": slug}).to_string();
            cx.call(Verb::Post, "/api/v1/teams", Some(&body), &[201], one_team)
                .await
        }
        TeamCmd::AddMember { team, username } => {
            let target = member_path(&team, &username)?;
            cx.call(Verb::Put, &target, None, &[200], one_team).await
        }
        TeamCmd::RemoveMember { team, username } => {
            let target = member_path(&team, &username)?;
            let line = format!("removed {username} from team {team}\n");
            cx.confirm(Verb::Delete, &target, &line).await
        }
    }
}

pub(crate) async fn run_workspace(
    cmd: WorkspaceCmd,
    cx: &mut Context<'_>,
) -> Result<(), AdminError> {
    match cmd {
        WorkspaceCmd::List { kind } => {
            // Kinds are slugs, so a checked one needs no encoding; `#`, `&` or `=` would
            // otherwise change the filter the server sees.
            let target = match kind {
                Some(kind) => format!("/api/v1/workspaces?kind={}", segment("kind", &kind)?),
                None => "/api/v1/workspaces".to_owned(),
            };
            cx.call_list(Verb::Get, &target, None, &[200], output::workspaces)
                .await
        }
        WorkspaceCmd::Create {
            team,
            slug,
            kind,
            name,
            default_branch,
            import,
        } => {
            let team = segment("team", &team)?;
            let body = json!({
                "name": name.as_deref().unwrap_or(&slug),
                "slug": slug,
                "kind": kind,
                "default_branch": default_branch,
                "seed": !import,
            })
            .to_string();
            let target = format!("/api/v1/teams/{team}/workspaces");
            cx.call(Verb::Post, &target, Some(&body), &[201], one_workspace)
                .await
        }
        WorkspaceCmd::Show { workspace } => {
            let target = format!("/api/v1/workspaces/{workspace}");
            cx.call(Verb::Get, &target, None, &[200], one_workspace)
                .await
        }
        WorkspaceCmd::Delete { workspace } => {
            let target = format!("/api/v1/workspaces/{workspace}");
            let line = format!("deleted workspace {workspace}\n");
            cx.confirm(Verb::Delete, &target, &line).await
        }
    }
}

fn member_path(team: &str, username: &str) -> Result<String, AdminError> {
    let team = segment("team", team)?;
    let username = segment("username", username)?;
    Ok(format!("/api/v1/teams/{team}/members/{username}"))
}

fn one_team(team: &Value) -> String {
    output::teams(std::slice::from_ref(team))
}

fn one_workspace(workspace: &Value) -> String {
    output::workspaces(std::slice::from_ref(workspace))
}
