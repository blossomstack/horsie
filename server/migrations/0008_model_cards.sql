-- Reference catalog of well-known models ("model cards"): the official
-- provider model id plus its token limits. Managed via /api/admin/model-cards,
-- searched by the Settings model form. Cards are prefill templates only —
-- configured models keep their own copies of these numbers.
CREATE TABLE model_cards (
    model_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    context_window INTEGER,
    max_tokens INTEGER,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
