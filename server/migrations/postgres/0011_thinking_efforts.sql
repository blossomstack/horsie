-- PostgreSQL mirror of migrations/sqlite/0011_thinking_efforts.sql.
--
-- Per-model thinking configuration.
--
-- `thinking_efforts` is a JSON array of canonical effort values
-- ("none","minimal","low","medium","high","xhigh","max") that this model
-- offers; a session picks one. `thinking_effort` is the default applied when a
-- session does not choose. `thinking_dialect` names the wire encoding — two
-- models on the same provider kind can need different shapes (Opus 4.8 takes
-- output_config.effort, Haiku 4.5 rejects effort entirely), so this is data,
-- not inference. NULL everywhere means "send no thinking control", preserving
-- existing behaviour.
--
-- Stored as TEXT holding JSON rather than PostgreSQL's jsonb: the column is
-- read and written as an opaque string by the same Rust code on both backends,
-- and nothing queries inside it.
--
-- Cards are reference data (what the provider supports); the `models` copy is
-- the deployment's editable menu, prefilled from the card.
ALTER TABLE model_cards ADD COLUMN thinking_efforts        TEXT;
ALTER TABLE model_cards ADD COLUMN default_thinking_effort TEXT;
ALTER TABLE model_cards ADD COLUMN thinking_dialect        TEXT;

ALTER TABLE models ADD COLUMN thinking_efforts TEXT;
ALTER TABLE models ADD COLUMN thinking_effort  TEXT;
ALTER TABLE models ADD COLUMN thinking_dialect TEXT;
