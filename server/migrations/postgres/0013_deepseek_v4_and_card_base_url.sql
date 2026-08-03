-- PostgreSQL mirror of migrations/sqlite/0013_deepseek_v4_and_card_base_url.sql.
--
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
-- the same reason thinking_dialect is (0011_thinking_efforts.sql). INTEGER
-- rather than BOOLEAN; see 0003_mcp.sql.
ALTER TABLE model_cards ADD COLUMN base_url                      TEXT;
ALTER TABLE model_cards ADD COLUMN forced_tools_disable_thinking INTEGER NOT NULL DEFAULT 0;
ALTER TABLE models      ADD COLUMN forced_tools_disable_thinking INTEGER NOT NULL DEFAULT 0;

-- `deepseek-chat` is superseded, and its seeded limits (128k/8192) never
-- matched the V4 models. As in 0012, this matches nothing on a fresh PostgreSQL
-- database; the replacement cards arrive through the bundled seed's
-- insert-if-missing pass on the next boot, exactly as they do on SQLite.
DELETE FROM model_cards WHERE model_id = 'deepseek-chat';
