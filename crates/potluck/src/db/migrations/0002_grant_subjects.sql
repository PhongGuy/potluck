-- Potluck schema version 2: a grant never outlives its user or team subject.
-- grants.subject_id points at users or teams by subject_kind, so no foreign key can guard it.

DELETE FROM grants
WHERE (subject_kind = 'user' AND subject_id NOT IN (SELECT id FROM users))
   OR (subject_kind = 'team' AND subject_id NOT IN (SELECT id FROM teams));
