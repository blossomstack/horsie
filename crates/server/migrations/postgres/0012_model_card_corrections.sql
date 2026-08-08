-- PostgreSQL mirror of migrations/sqlite/0012_model_card_corrections.sql.
--
-- On SQLite this repaired rows that the bundled catalog had already seeded with
-- wrong limits. A PostgreSQL deployment is necessarily new, so there are no
-- such rows and every statement below matches nothing. It is kept as a real
-- file at the same version anyway: the two directories stay aligned
-- version-for-version, which is what makes the parity test a useful guard
-- rather than a list of exceptions.
--
-- Cards are prefill templates and are never linked to configured models
-- (0008_model_cards.sql), so deleting one cannot break a running deployment.

-- Wrong limits: both are far larger than the original seed claimed.
UPDATE model_cards
   SET context_window = 1000000, max_tokens = 128000,
       updated_at = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')
 WHERE model_id = 'claude-sonnet-4-6';

UPDATE model_cards
   SET context_window = 200000, max_tokens = 64000,
       updated_at = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')
 WHERE model_id = 'claude-haiku-4-5';

-- Superseded entries. Removed so the prefill menu reflects what is current; an
-- operator who still wants one can re-add it via /api/admin/model-cards.
DELETE FROM model_cards WHERE model_id IN ('o1', 'claude-opus-4-1', 'gpt-4o');
