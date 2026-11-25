//! What happens after git accepts a push: the one-transaction record, the audit line, the notices.

use std::path::PathBuf;

use crate::api::AppState;
use crate::db::Db;
use crate::db::overrides::Relaxation;
use crate::git::GitEngine;
use crate::git::access::PushContext;
use crate::git::push::RefMove;
use crate::kinds::{self, RuleSet};
use crate::role::Role;
use crate::secret_check::git::{Git, RefUpdate};
use crate::secret_check::scan::Rejection;
use crate::unicode::inert;

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
        .record_push(workspace_id, override_id, &moves, pusher)
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
            error = %inert(&err.to_string()),
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

/// Re-checks an accepted admin or override push on a blocking thread: the safety relaxations
/// of an admin push without an armed override, in a kind with a rule set, and for both any
/// rewrite of the default branch, any ref written outside branches and tags, and any tag moved
/// or deleted. Any other push, or one with no updates, returns at once.
pub(crate) async fn recheck_admin_push(
    state: &AppState,
    push: &PushContext,
    repo: PathBuf,
    role: Role,
    updates: Vec<RefUpdate>,
) {
    let armed = push.armed_override().is_some();
    if (role != Role::Admin && !armed) || updates.is_empty() {
        return;
    }
    let rules = push
        .workspace_kind()
        .and_then(kinds::by_name)
        .filter(|_| role == Role::Admin && !armed);
    recheck(state.git(), state.db(), push, rules, repo, updates).await;
}

/// The re-check itself over `repo` on a blocking thread: `log_accepted_relaxations` when `rules`
/// is given, then `log_default_branch_rewrites`, `log_foreign_refs` and `log_tag_rewrites`,
/// holding one of the engine's process slots throughout and with each git child under the
/// engine's run limit. It waits for a slot without a timeout: the push is accepted, so its audit
/// line should not drop under load. A re-check that cannot finish logs the warn line. The
/// findings of the last three then go to `Db::record_relaxations` in one call; a failed write
/// logs at error and a workspace already gone logs at warn, and neither fails anything.
pub(crate) async fn recheck(
    engine: &GitEngine,
    db: &Db,
    push: &PushContext,
    rules: Option<&'static dyn RuleSet>,
    repo: PathBuf,
    updates: Vec<RefUpdate>,
) {
    let workspace_id = push.workspace_id();
    let pusher = push.pusher();
    let slot = engine.slot().await;
    let git = Git::new(engine.repos().git().to_path_buf(), Some(repo))
        .with_max_runtime(engine.max_runtime());
    let owned = pusher.to_owned();
    let override_id = push.armed_override();
    let default_branch = push.workspace_default_branch().map(str::to_owned);
    let done = match slot {
        Some(slot) => tokio::task::spawn_blocking(move || {
            let _slot = slot;
            if let Some(rules) = rules {
                crate::secret_check::log_accepted_relaxations(
                    workspace_id,
                    &owned,
                    rules,
                    &updates,
                    &git,
                );
            }
            let mut found = crate::secret_check::log_default_branch_rewrites(
                workspace_id,
                &owned,
                override_id,
                default_branch.as_deref(),
                &updates,
                &git,
            );
            found.extend(crate::secret_check::log_foreign_refs(
                workspace_id,
                &owned,
                override_id,
                &updates,
            ));
            found.extend(crate::secret_check::log_tag_rewrites(
                workspace_id,
                &owned,
                override_id,
                &updates,
            ));
            relaxations(&found, &updates)
        })
        .await
        .ok(),
        None => None,
    };
    let Some(found) = done else {
        tracing::warn!(
            workspace_id,
            pusher,
            "could not re-check an accepted admin push"
        );
        return;
    };
    if found.is_empty() {
        return;
    }
    match db
        .record_relaxations(workspace_id, pusher, override_id, &found)
        .await
    {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            workspace_id,
            pusher,
            "workspace gone before its relaxations were recorded"
        ),
        // The info lines above stay the record; each failed row is named here.
        Err(err) => {
            for r in &found {
                tracing::error!(
                    workspace_id,
                    pusher,
                    "ref" = r.ref_name(),
                    rule = r.rule(),
                    error = %inert(&err.to_string()),
                    "could not record a safety relaxation"
                );
            }
        }
    }
}

/// Pairs each finding with the update for its ref, so the row carries the shas it moved
/// between. Ref names were checked as UTF-8 before the push ran.
fn relaxations(found: &[Rejection], updates: &[RefUpdate]) -> Vec<Relaxation> {
    found
        .iter()
        .filter_map(|r| {
            let update = updates.iter().find(|u| u.name() == r.ref_name())?;
            Some(Relaxation::new(
                &String::from_utf8_lossy(r.ref_name()),
                update.old().as_str(),
                update.new().as_str(),
                r.finding().rule(),
            ))
        })
        .collect()
}
