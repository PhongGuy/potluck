-- Potluck schema version 1: accounts, teams, workspaces, grants and overrides.
-- Timestamps are RFC 3339 UTC text; booleans are 0 or 1; token digests are 32 raw bytes.

CREATE TABLE users (
    id            INTEGER PRIMARY KEY,
    username      TEXT    NOT NULL UNIQUE,
    display_name  TEXT    NOT NULL,
    email         TEXT,
    password_hash TEXT,
    server_admin  INTEGER NOT NULL DEFAULT 0 CHECK (server_admin IN (0, 1)),
    disabled      INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0, 1)),
    created_at    TEXT    NOT NULL
) STRICT;

CREATE TABLE tokens (
    id           INTEGER PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users(id),
    name         TEXT    NOT NULL,
    prefix       TEXT    NOT NULL,
    digest       BLOB    NOT NULL UNIQUE,
    created_at   TEXT    NOT NULL,
    last_used_at TEXT,
    expires_at   TEXT
) STRICT;
CREATE INDEX tokens_user ON tokens(user_id);

CREATE TABLE teams (
    id   INTEGER PRIMARY KEY,
    slug TEXT    NOT NULL UNIQUE,
    name TEXT    NOT NULL
) STRICT;

CREATE TABLE team_members (
    team_id INTEGER NOT NULL REFERENCES teams(id),
    user_id INTEGER NOT NULL REFERENCES users(id),
    PRIMARY KEY (team_id, user_id)
) STRICT;

CREATE TABLE workspaces (
    id             INTEGER PRIMARY KEY,
    team_id        INTEGER NOT NULL REFERENCES teams(id),
    slug           TEXT    NOT NULL,
    name           TEXT    NOT NULL,
    kind           TEXT    NOT NULL CHECK (kind IN ('apinata', 'dbento')),
    default_branch TEXT    NOT NULL,
    import_pending INTEGER NOT NULL DEFAULT 0 CHECK (import_pending IN (0, 1)),
    created_at     TEXT    NOT NULL,
    UNIQUE (team_id, slug)
) STRICT;

CREATE TABLE grants (
    workspace_id INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    subject_kind TEXT    NOT NULL CHECK (subject_kind IN ('user', 'team')),
    subject_id   INTEGER NOT NULL,
    role         TEXT    NOT NULL CHECK (role IN ('read', 'write', 'admin')),
    PRIMARY KEY (workspace_id, subject_kind, subject_id)
) STRICT;
CREATE INDEX grants_subject ON grants(subject_kind, subject_id);

CREATE TABLE overrides (
    id            INTEGER PRIMARY KEY,
    workspace_id  INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    for_user_id   INTEGER NOT NULL REFERENCES users(id),
    reason        TEXT    NOT NULL,
    armed_by      TEXT    NOT NULL,
    armed_at      TEXT    NOT NULL,
    consumed_at   TEXT,
    consumed_refs TEXT
) STRICT;
CREATE INDEX overrides_armed ON overrides(workspace_id, for_user_id) WHERE consumed_at IS NULL;
