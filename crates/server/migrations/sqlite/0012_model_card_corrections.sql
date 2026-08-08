-- The original bundled catalog shipped incorrect limits for two current models
-- and several entries that are superseded. Card seeding is insert-if-missing, so
-- it can never fix a row that already exists — corrections have to happen here.
--
-- Cards are prefill templates and are never linked to configured models
-- (0008_model_cards.sql), so deleting one cannot break a running deployment.

-- Wrong limits: both are far larger than the original seed claimed.
UPDATE model_cards
   SET context_window = 1000000, max_tokens = 128000, updated_at = datetime('now')
 WHERE model_id = 'claude-sonnet-4-6';

UPDATE model_cards
   SET context_window = 200000, max_tokens = 64000, updated_at = datetime('now')
 WHERE model_id = 'claude-haiku-4-5';

-- Superseded entries. Removed so the prefill menu reflects what is current; an
-- operator who still wants one can re-add it via /api/admin/model-cards.
DELETE FROM model_cards WHERE model_id IN ('o1', 'claude-opus-4-1', 'gpt-4o');
