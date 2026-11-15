//! The contract's JSON objects: `User`, `Token`, `Identity`, `Team`, `Workspace`, `Grant`.

use serde::Serialize;

use crate::db::accounts::{TokenRecord, UserRecord};
use crate::db::grants::{GrantRecord, Subject};
use crate::db::teams::TeamRecord;
use crate::db::workspaces::WorkspaceRecord;
use crate::role::Role;

/// The `User` object: `{username, display_name, email|null, server_admin, disabled, created_at}`.
/// The password hash is never part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct UserJson {
    username: String,
    display_name: String,
    email: Option<String>,
    server_admin: bool,
    disabled: bool,
    created_at: String,
}

impl From<&UserRecord> for UserJson {
    fn from(user: &UserRecord) -> Self {
        UserJson {
            username: user.username().to_owned(),
            display_name: user.display_name().to_owned(),
            email: user.email().map(str::to_owned),
            server_admin: user.server_admin(),
            disabled: user.disabled(),
            created_at: user.created_at().to_owned(),
        }
    }
}

/// The `Token` object: `{id, name, prefix, created_at, last_used_at|null, expires_at|null}`.
/// The digest is never part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct TokenJson {
    id: i64,
    name: String,
    prefix: String,
    created_at: String,
    last_used_at: Option<String>,
    expires_at: Option<String>,
}

impl From<&TokenRecord> for TokenJson {
    fn from(token: &TokenRecord) -> Self {
        TokenJson {
            id: token.id(),
            name: token.name().to_owned(),
            prefix: token.prefix().to_owned(),
            created_at: token.created_at().to_owned(),
            last_used_at: token.last_used_at().map(str::to_owned),
            expires_at: token.expires_at().map(str::to_owned),
        }
    }
}

/// The `Identity` object: `{name, email}`, the author and committer of a user's commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct IdentityJson {
    name: String,
    email: String,
}

/// `display_name` (the username when blank) and the email, else
/// `{username}@users.noreply.{host of public_url}`.
pub fn identity(user: &UserRecord, public_url: &str) -> IdentityJson {
    let display = user.display_name().trim();
    let name = if display.is_empty() {
        user.username()
    } else {
        display
    };
    let email = user.email().map_or_else(
        || {
            format!(
                "{}@users.noreply.{}",
                user.username(),
                noreply_host(public_url)
            )
        },
        str::to_owned,
    );
    IdentityJson {
        name: name.to_owned(),
        email,
    }
}

/// The host of `public_url` for `@users.noreply.{host}` addresses; `localhost` if it cannot be
/// read.
pub fn noreply_host(public_url: &str) -> String {
    // The public URL is validated at startup; the fallback host only guards against a panic.
    url::Url::parse(public_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "localhost".to_owned())
}

/// The `Team` object: `{slug, name, members: [username]}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct TeamJson {
    slug: String,
    name: String,
    members: Vec<String>,
}

impl TeamJson {
    /// `team` with its members' usernames, sorted as the database lists them.
    pub fn new(team: &TeamRecord, members: Vec<String>) -> Self {
        TeamJson {
            slug: team.slug().to_owned(),
            name: team.name().to_owned(),
            members,
        }
    }
}

/// The `Workspace` object: `{team, slug, name, kind, default_branch, role, git_url, events_url}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct WorkspaceJson {
    team: String,
    slug: String,
    name: String,
    kind: String,
    default_branch: String,
    role: &'static str,
    git_url: String,
    events_url: String,
}

impl WorkspaceJson {
    /// `git_url` is `{public_url}/git/{team}/{ws}.git` and `events_url` is
    /// `{public_url}/api/v1/workspaces/{team}/{ws}/events`.
    pub fn new(ws: &WorkspaceRecord, role: Role, public_url: &str) -> Self {
        let (team, slug) = (ws.team_slug(), ws.slug());
        WorkspaceJson {
            team: team.to_owned(),
            slug: slug.to_owned(),
            name: ws.name().to_owned(),
            kind: ws.kind().to_owned(),
            default_branch: ws.default_branch().to_owned(),
            role: role.as_str(),
            git_url: format!("{public_url}/git/{team}/{slug}.git"),
            events_url: format!("{public_url}/api/v1/workspaces/{team}/{slug}/events"),
        }
    }
}

/// A grant's subject: `{kind: "user"|"team", name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SubjectJson {
    kind: &'static str,
    name: String,
}

impl From<&Subject> for SubjectJson {
    fn from(subject: &Subject) -> Self {
        SubjectJson {
            kind: subject.kind(),
            name: subject.name().to_owned(),
        }
    }
}

/// The `Grant` object: `{subject: {kind, name}, role}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GrantJson {
    subject: SubjectJson,
    role: &'static str,
}

impl From<&GrantRecord> for GrantJson {
    fn from(grant: &GrantRecord) -> Self {
        GrantJson {
            subject: SubjectJson::from(grant.subject()),
            role: grant.role().as_str(),
        }
    }
}
