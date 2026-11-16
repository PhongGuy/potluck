-- Potluck schema version 2: a grant never outlives its user or team subject.
-- grants.subject_id points at users or teams by subject_kind, so no foreign key can guard it.

DELETE FROM grants
WHERE (subject_kind = 'user' AND subject_id NOT IN (SELECT id FROM users))
   OR (subject_kind = 'team' AND subject_id NOT IN (SELECT id FROM teams));

-- From here on the deleting statement removes the subject's grants in its own transaction,
-- so a reused rowid can never inherit a deleted subject's role.
CREATE TRIGGER users_delete_grants AFTER DELETE ON users BEGIN
    DELETE FROM grants WHERE subject_kind = 'user' AND subject_id = OLD.id;
END;

CREATE TRIGGER teams_delete_grants AFTER DELETE ON teams BEGIN
    DELETE FROM grants WHERE subject_kind = 'team' AND subject_id = OLD.id;
END;
