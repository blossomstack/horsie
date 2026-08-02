-- DeepSeek V4 support, plus a `base_url` on model cards.
--
-- `base_url` records where a model is officially served. It is reference data
-- only: nothing on the server reads it, and cards remain prefill templates that
-- are never linked to configured models (0008_model_cards.sql). Request routing
-- still reads providers.base_url alone.
--
-- `forced_tools_disable_thinking` marks backends that reject a pinned
-- tool_choice while thinking is enabled. DeepSeek returns 400 "Thinking mode
-- does not support this tool_choice" for tool_choice=required and for a named
-- function whenever thinking is on. Stored per model rather than inferred, for
-- the same reason thinking_dialect is (0011_thinking_efforts.sql).
ALTER TABLE model_cards ADD COLUMN base_url                      TEXT;
ALTER TABLE model_cards ADD COLUMN forced_tools_disable_thinking INTEGER NOT NULL DEFAULT 0;
ALTER TABLE models      ADD COLUMN forced_tools_disable_thinking INTEGER NOT NULL DEFAULT 0;

-- `deepseek-chat` is superseded, and its seeded limits (128k/8192) never
-- matched the V4 models. Deleting a card cannot affect a running deployment.
--
-- Only the deletion belongs here. The replacement cards (deepseek-v4-flash,
-- deepseek-v4-pro) are new ids that exist in no database yet, so the bundled
-- seed's insert-if-missing pass adds them on the next boot for existing and
-- fresh installs alike. Migration 0012 had to write rows directly because it
-- was correcting ids that already existed; that does not apply here, and
-- seeding from a migration would also plant rows in every fresh test database.
DELETE FROM model_cards WHERE model_id = 'deepseek-chat';
