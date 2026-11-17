//! What happens after git accepts a push: the one-transaction record, the audit line, the notices.

use std::path::PathBuf;

use crate::api::AppState;
use crate::git::access::PushContext;
use crate::git::push::RefMove;
use crate::kinds;
use crate::role::Role;
use crate::secret_check::git::{Git, RefUpdate};

/// Records `moves` for `push`: `Db::record_push`, one info line (`workspace_id`, `pusher`,
/// `override_id`, `override_consumed`, `import_cleared`, `refs` as `ref old..new` joined by
/// `, `), then `Notices::publish`. An empty `moves` logs at debug and does nothing else. A
/// database failure is logged at error and the notices still go out: the refs have moved. An
/// `override_id` that was not consumed (already consumed by a concurrent push) is logged at warn
/// as "override already consumed; this push was accepted without a scan".
pub(crate) async fn record(state: AppState, push: PushContext, moves: Vec<RefMove>) {
    let workspace_id = push.workspace_id();
    let pusher = push.pusher();
    let override_id = push.armed_override();
    if moves.is_empty() {
        tracing::debug!(workspace_id, pusher, "push moved no refs");
        return;
    }
    let refs = moves
        .iter()
        .map(|m| format!("{} {}..{}", m.ref_name(), m.old(), m.new_sha()))
        .collect::<Vec<_>>()
        .join(", ");
    match state
        .db()
        .record_push(workspace_id, override_id, &moves)
        .await
    {
        Ok(done) => {
            tracing::info!(
                workspace_id,
                pusher,
                override_id,
                override_consumed = done.override_consumed(),
                import_cleared = done.import_cleared(),
                refs,
                "push accepted"
            );
            if override_id.is_some() && !done.override_consumed() {
                tracing::warn!(
                    workspace_id,
                    pusher,
                    override_id,
                    "override already consumed; this push was accepted without a scan"
                );
            }
        }
        Err(err) => tracing::error!(
            workspace_id,
            pusher,
            override_id,
            refs,
            error = %err,
            "could not record an accepted push"
        ),
    }
    // Clients must fetch whether or not the record landed.
    state.notices().publish(workspace_id, &moves);
}

/// The moves as the hook saw them. A move that does not parse is logged at warn and skipped, so
/// a bad line never panics the reaper task.
pub(crate) fn ref_updates(moves: &[RefMove]) -> Vec<RefUpdate> {
    moves
        .iter()
        .filter_map(|m| {
            let line = format!("{} {} {}", m.old(), m.new_sha(), m.ref_name());
            RefUpdate::parse_line(line.as_bytes())
                .inspect_err(|_| tracing::warn!(ref_name = m.ref_name(), "unparsable ref move"))
                .ok()
        })
        .collect()
}

/// Logs the safety relaxations an admin's accepted push carried, through
/// `secret_check::log_accepted_relaxations` on a blocking thread. Only an `Admin` push without an
/// armed override, in a kind with a rule set, is re-checked; anything else returns at once.
pub(crate) async fn recheck_admin_push(
    state: &AppState,
    push: &PushContext,
    repo: PathBuf,
    role: Role,
    updates: Vec<RefUpdate>,
) {
    let rules = match push.workspace_kind().and_then(kinds::by_name) {
        Some(rules) if role == Role::Admin && push.armed_override().is_none() => rules,
        _ => return,
    };
    if updates.is_empty() {
        return;
    }
    let git = Git::new(state.git().repos().git().to_path_buf(), Some(repo));
    let workspace_id = push.workspace_id();
    let pusher = push.pusher().to_owned();
    let done = tokio::task::spawn_blocking(move || {
        crate::secret_check::log_accepted_relaxations(workspace_id, &pusher, rules, &updates, &git);
    })
    .await;
    if done.is_err() {
        tracing::warn!(
            workspace_id,
            pusher = push.pusher(),
            "could not re-check an accepted admin push"
        );
    }
}
