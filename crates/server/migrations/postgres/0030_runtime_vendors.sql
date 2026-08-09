-- PostgreSQL mirror of migrations/sqlite/0028_runtime_vendors.sql.
--
-- Runtime vendors configured in settings, as opposed to the ones that dial in.
--
-- A `horsie connect` vendor announces itself over a websocket and is never
-- written down; a cloud vendor has nowhere to dial from, so its configuration
-- is the only record that it exists. This table is what the server rebuilds
-- those from at boot.
--
-- `kind` selects how `settings` is read — the JSON is that kind's own shape,
-- not a tagged union, so the column stays the single discriminator. A new
-- vendor kind is a new match arm, not a schema change.
--
-- `credential` is the vendor API token. It is stored in the clear, exactly as
-- provider API keys in `providers` are: the database is already the trust
-- boundary for this server, and encrypting one column while the next holds a
-- plaintext key would buy nothing.

CREATE TABLE runtime_vendors (
    user_id    TEXT NOT NULL,
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL,                 -- 'fly'
    settings   TEXT NOT NULL DEFAULT '{}',    -- JSON, shaped by `kind`
    credential TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,                 -- unix epoch seconds
    updated_at TEXT NOT NULL,                 -- unix epoch seconds
    PRIMARY KEY (user_id, name)
);
