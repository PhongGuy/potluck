//! `potluck admin audit`: the server's audit log, newest first.

use super::client::Verb;
use super::{AdminError, Context, output};

/// Filters for `potluck admin audit`; each one set becomes one query parameter.
#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct AuditArgs {
    /// Only rows by this username.
    #[arg(long)]
    actor: Option<String>,
    /// Only rows on this workspace, as `{team}/{ws}`.
    #[arg(long)]
    workspace: Option<String>,
    /// Only rows of this action, such as `grant.put`.
    #[arg(long)]
    action: Option<String>,
    /// Only rows at or after this RFC 3339 time.
    #[arg(long)]
    since: Option<String>,
    /// Only rows before this RFC 3339 time.
    #[arg(long)]
    until: Option<String>,
    /// Only rows with a smaller id: the next page after the last id shown.
    #[arg(long)]
    before: Option<i64>,
    /// At most this many rows (1 to 1000).
    #[arg(long)]
    limit: Option<u32>,
}

pub(crate) async fn run(args: AuditArgs, cx: &mut Context<'_>) -> Result<(), AdminError> {
    let target = target(&args);
    cx.call_list(Verb::Get, &target, None, &[200], output::audit)
        .await
}

// Form encoding keeps a `+` in an offset stamp from reaching the server as a space.
fn target(args: &AuditArgs) -> String {
    let pairs = [
        ("actor", args.actor.clone()),
        ("workspace", args.workspace.clone()),
        ("action", args.action.clone()),
        ("since", args.since.clone()),
        ("until", args.until.clone()),
        ("before", args.before.map(|n| n.to_string())),
        ("limit", args.limit.map(|n| n.to_string())),
    ];
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        if let Some(value) = value {
            query.append_pair(key, &value);
        }
    }
    let query = query.finish();
    if query.is_empty() {
        "/api/v1/audit".to_owned()
    } else {
        format!("/api/v1/audit?{query}")
    }
}
