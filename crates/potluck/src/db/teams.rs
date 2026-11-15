//! The team and membership rows of `server.db`, each query one job on the database thread.

use rusqlite::{OptionalExtension, Row, params};

use super::{Db, DbError, conflict_on_unique};
use crate::slug::Slug;

/// A `teams` row.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TeamRecord {
    id: i64,
    slug: String,
    name: String,
}

impl TeamRecord {
    /// The row id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The unique slug, `{team}` in URLs and paths.
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// The display name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

fn team_from_row(row: &Row<'_>) -> rusqlite::Result<TeamRecord> {
    Ok(TeamRecord {
        id: row.get(0)?,
        slug: row.get(1)?,
        name: row.get(2)?,
    })
}

impl Db {
    /// Inserts; `Conflict` ("team slug {slug} is taken") when the slug exists.
    pub async fn create_team(&self, slug: &Slug, name: &str) -> Result<TeamRecord, DbError> {
        let slug = slug.as_str().to_owned();
        let name = name.to_owned();
        // The UNIQUE constraint is the duplicate check, so a writer on another connection is
        // caught too.
        self.call(move |conn| {
            conn.execute(
                "INSERT INTO teams (slug, name) VALUES (?1, ?2)",
                params![slug, name],
            )
            .map_err(|e| conflict_on_unique(e, || format!("team slug {slug} is taken")))?;
            Ok(TeamRecord {
                id: conn.last_insert_rowid(),
                slug,
                name,
            })
        })
        .await
    }

    /// Exact match on `slug`.
    pub async fn team_by_slug(&self, slug: &str) -> Result<Option<TeamRecord>, DbError> {
        let slug = slug.to_owned();
        self.call(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT id, slug, name FROM teams WHERE slug = ?1",
                    [slug],
                    team_from_row,
                )
                .optional()?)
        })
        .await
    }

    /// Every team, by slug.
    pub async fn list_teams(&self) -> Result<Vec<TeamRecord>, DbError> {
        self.call(|conn| {
            let mut stmt = conn.prepare("SELECT id, slug, name FROM teams ORDER BY slug")?;
            let teams = stmt
                .query_map([], team_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(teams)
        })
        .await
    }

    /// The members' usernames, sorted; empty for an unknown team.
    pub async fn team_members(&self, team_id: i64) -> Result<Vec<String>, DbError> {
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT u.username FROM team_members m JOIN users u ON u.id = m.user_id \
                 WHERE m.team_id = ?1 ORDER BY u.username",
            )?;
            let names = stmt
                .query_map([team_id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(names)
        })
        .await
    }

    /// Adds the user; `Ok(false)` when already a member. Both ids must exist (foreign keys).
    pub async fn add_member(&self, team_id: i64, user_id: i64) -> Result<bool, DbError> {
        self.call(move |conn| {
            let changed = conn.execute(
                "INSERT OR IGNORE INTO team_members (team_id, user_id) VALUES (?1, ?2)",
                [team_id, user_id],
            )?;
            Ok(changed > 0)
        })
        .await
    }

    /// Removes the user; `Ok(false)` when not a member.
    pub async fn remove_member(&self, team_id: i64, user_id: i64) -> Result<bool, DbError> {
        self.call(move |conn| {
            let changed = conn.execute(
                "DELETE FROM team_members WHERE team_id = ?1 AND user_id = ?2",
                [team_id, user_id],
            )?;
            Ok(changed > 0)
        })
        .await
    }
}
