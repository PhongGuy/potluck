-- Potluck schema version 3: the append-only audit log. No foreign keys: a row outlives what it names.
CREATE TABLE audit (
    id          INTEGER PRIMARY KEY,
    at          TEXT    NOT NULL,
    action      TEXT    NOT NULL,
    actor_kind  TEXT    NOT NULL CHECK (actor_kind IN ('user', 'system', 'anonymous')),
    actor_id    INTEGER,
    actor_name  TEXT,
    target_kind TEXT,
    target_id   INTEGER,
    target_name TEXT,
    workspace   TEXT,
    client_ip   TEXT,
    detail      TEXT
) STRICT;
CREATE INDEX audit_actor     ON audit(actor_name, id);
CREATE INDEX audit_workspace ON audit(workspace, id);
CREATE INDEX audit_action    ON audit(action, id);
CREATE INDEX audit_at        ON audit(at);
