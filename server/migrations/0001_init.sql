-- Session-server settings store. Holds the runtime-editable configuration the
-- Settings UI owns (providers, models, vendors, default vendor). Deployment /
-- bootstrap config (storage, sandbox, runtime, hackamore, the DB location) lives
-- in config.json/env and never overlaps with these tables.

CREATE TABLE providers (
    name        TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    base_url    TEXT,
    api_key_env TEXT,
    api_key     TEXT
);

CREATE TABLE models (
    alias      TEXT PRIMARY KEY,
    provider   TEXT NOT NULL,
    model_id   TEXT NOT NULL,
    max_tokens INTEGER
);

-- Configurable runtime vendors (the built-in `local` vendor is not stored here).
-- `kind` selects the vendor type ("velos" today); `config` is its kind-specific
-- config as JSON. A new vendor kind is a new `kind` value — no schema change.
CREATE TABLE vendors (
    name   TEXT PRIMARY KEY,
    kind   TEXT NOT NULL,
    config TEXT NOT NULL
);

-- Single-row key/value bag for scalar settings (currently `default_vendor`).
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
