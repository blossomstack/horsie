-- PostgreSQL mirror of migrations/sqlite/0008_model_cards.sql.
--
-- Reference catalog of well-known models ("model cards"): the official
-- provider model id plus its token limits. Managed via /api/admin/model-cards,
-- searched by the Settings model form. Cards are prefill templates only —
-- configured models keep their own copies of these numbers.
--
-- The timestamps are TEXT on both backends, and the default has to produce the
-- same shape SQLite's datetime('now') does ("YYYY-MM-DD HH:MM:SS", UTC), since
-- the same Rust code parses either.
CREATE TABLE model_cards (
    model_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    context_window INTEGER,
    max_tokens INTEGER,
    created_at TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')),
    updated_at TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS'))
);
