//! `GET /audit` for server admins: the audit log, newest first, filtered and paged by id.

use axum::Json;
use axum::extract::{RawQuery, State};

use super::AppState;
use super::auth::Authenticated;
use super::error::{ApiError, ApiResult};
use super::objects::AuditJson;
use crate::clock;
use crate::db::audit::AuditFilter;

/// `GET /audit?actor=&workspace=&action=&since=&until=&before=&limit=` (server admin or
/// `System`): `[Audit]`. A bad `since`, `until`, `before` or `limit` is 422 naming it.
pub(super) async fn list(
    State(state): State<AppState>,
    auth: Authenticated,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Vec<AuditJson>>> {
    auth.require_server_admin()?;
    let filter = parse(query.as_deref().unwrap_or(""))?;
    let rows = state.db().audit(filter).await?;
    Ok(Json(rows.iter().map(AuditJson::from).collect()))
}

// Each known parameter narrows the filter as it comes, so a repeated one ends on its last
// value; unknown parameters are ignored.
fn parse(query: &str) -> Result<AuditFilter, ApiError> {
    let mut filter = AuditFilter::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        filter = match &*key {
            "actor" => filter.actor(&value),
            "workspace" => filter.workspace(&value),
            "action" => filter.action(&value),
            "since" => filter.since(&stamp("since", &value)?),
            "until" => filter.until(&stamp("until", &value)?),
            "before" => filter.before(value.parse().map_err(|_| {
                ApiError::invalid_value("before", "before must be an audit row id")
            })?),
            "limit" => filter.limit(
                value
                    .parse()
                    .ok()
                    .filter(|n| (1..=1000).contains(n))
                    .ok_or_else(|| ApiError::invalid_value("limit", "limit must be 1 to 1000"))?,
            ),
            _ => filter,
        };
    }
    Ok(filter)
}

fn stamp(field: &str, value: &str) -> Result<String, ApiError> {
    clock::normalize_rfc3339(value)
        .ok_or_else(|| ApiError::invalid_value(field, format!("{field} must be an RFC 3339 time")))
}
