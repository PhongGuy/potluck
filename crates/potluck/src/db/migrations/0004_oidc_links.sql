-- Potluck schema version 4: which IdP subject signs in as which user, one per user per issuer.
CREATE TABLE oidc_links (
    issuer    TEXT    NOT NULL,
    subject   TEXT    NOT NULL,
    user_id   INTEGER NOT NULL REFERENCES users(id),
    linked_at TEXT    NOT NULL,
    PRIMARY KEY (issuer, subject),
    UNIQUE (issuer, user_id)
) STRICT;
