//! What happens after git accepts a push: the one-transaction record, the audit line, the notices.

use crate::api::AppState;
use crate::git::access::PushContext;
use crate::git::push::RefMove;

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
