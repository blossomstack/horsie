-- Every durable row belongs to a user.
--
-- `user_id` is the first half of the key wherever the key was a natural name,
-- so two accounts may hold the same provider, model, skill bundle or memory
-- space without colliding.
--
-- SQLite cannot alter a primary key, a UNIQUE constraint, or a CHECK, so every
-- table here is a rebuild: create, copy, drop, rename. Sixteen of them.
--
-- Deliberately NO DEFAULT on `user_id`. A default would make an INSERT that
-- forgets the scope land silently in another account's data; without one it is
-- a NOT NULL violation, which is a test failure rather than a leak.
--
-- Existing rows backfill to '1' -- the bootstrap account, whose id 0023 set to
-- the text of the integer it had. Accounts created after that migration get a
-- random id from `create_user`.
--
-- No REFERENCES clauses: `PRAGMA foreign_keys` is never enabled in `open_pool`,
-- so a declared constraint would be silently ignored. See 0009_memory.sql.
--
-- Three tables are deliberately NOT scoped:
--   * `github_app`      -- one App registration per deployment (one callback
--                          URL, one client id, one private key). Accounts
--                          *install* it; that is `github_credentials`.
--   * `journal_events`, `journal_snapshots` -- reached only by `log_id`, which
--                          comes from the scoped `journal_logs` lookup. Adding
--                          `user_id` would widen `PRIMARY KEY (log_id, seq)`,
--                          the WITHOUT ROWID key that makes replay a contiguous
--                          range scan, to duplicate what the parent enforces.
-- And `auth_users`/`auth_tokens`/`auth_device_codes` define the scope rather
-- than living inside it.

-- `providers`
CREATE TABLE providers_new (
    user_id     TEXT NOT NULL,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    base_url    TEXT,
    api_key     TEXT,
    keep_thinking_signature INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, name)
);
INSERT INTO providers_new SELECT '1', * FROM providers;
DROP TABLE providers;
ALTER TABLE providers_new RENAME TO providers;

-- `models`
CREATE TABLE models_new (
    user_id    TEXT NOT NULL,
    alias      TEXT NOT NULL,
    provider   TEXT NOT NULL,
    model_id   TEXT NOT NULL,
    max_tokens INTEGER,
    context_window INTEGER,
    thinking_efforts TEXT,
    thinking_effort  TEXT,
    thinking_dialect TEXT,
    forced_tools_disable_thinking INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, alias)
);
INSERT INTO models_new SELECT '1', * FROM models;
DROP TABLE models;
ALTER TABLE models_new RENAME TO models;

-- `settings`
CREATE TABLE settings_new (
    user_id TEXT NOT NULL,
    key     TEXT NOT NULL,
    value   TEXT NOT NULL,
    PRIMARY KEY (user_id, key)
);
INSERT INTO settings_new SELECT '1', * FROM settings;
DROP TABLE settings;
ALTER TABLE settings_new RENAME TO settings;

-- `mcp_servers`
CREATE TABLE mcp_servers_new (
    user_id      TEXT NOT NULL,
    name         TEXT NOT NULL,
    url          TEXT NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 0,
    auth_kind    TEXT NOT NULL,
    bearer_token TEXT,
    tool_count   INTEGER,
    last_error   TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    oauth_client_id     TEXT,
    oauth_client_secret TEXT,
    oauth_access_token  TEXT,
    oauth_refresh_token TEXT,
    oauth_expires_at    TEXT,
    oauth_meta          TEXT,
    PRIMARY KEY (user_id, name)
);
INSERT INTO mcp_servers_new SELECT '1', * FROM mcp_servers;
DROP TABLE mcp_servers;
ALTER TABLE mcp_servers_new RENAME TO mcp_servers;

-- `plugins`
CREATE TABLE plugins_new (
    user_id         TEXT NOT NULL,
    name            TEXT NOT NULL,
    source_kind     TEXT NOT NULL,
    source_url      TEXT NOT NULL,
    source_ref      TEXT,
    version         TEXT,
    description     TEXT,
    skill_count     INTEGER NOT NULL DEFAULT 0,
    has_hooks       INTEGER NOT NULL DEFAULT 0,
    artifact_hash   TEXT NOT NULL,
    artifact_size   INTEGER NOT NULL,
    enabled_default INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    source_subpath  TEXT,
    marketplace     TEXT,
    marketplace_entry TEXT,
    PRIMARY KEY (user_id, name)
);
INSERT INTO plugins_new SELECT '1', * FROM plugins;
DROP TABLE plugins;
ALTER TABLE plugins_new RENAME TO plugins;

-- `memory_spaces`
CREATE TABLE memory_spaces_new (
    user_id     TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (user_id, name)
);
INSERT INTO memory_spaces_new SELECT '1', * FROM memory_spaces;
DROP TABLE memory_spaces;
ALTER TABLE memory_spaces_new RENAME TO memory_spaces;

-- `agents`
CREATE TABLE agents_new (
    user_id         TEXT NOT NULL,
    name            TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    model           TEXT NOT NULL,
    repos           TEXT NOT NULL DEFAULT '[]',
    plugins         TEXT NOT NULL DEFAULT '[]',
    mcp_servers     TEXT NOT NULL DEFAULT '[]',
    memory_spaces   TEXT NOT NULL DEFAULT '[]',
    thinking_effort TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (user_id, name)
);
INSERT INTO agents_new SELECT '1', * FROM agents;
DROP TABLE agents;
ALTER TABLE agents_new RENAME TO agents;

-- `routines`
CREATE TABLE routines_new (
    user_id         TEXT    NOT NULL,
    name            TEXT    NOT NULL,
    description     TEXT    NOT NULL DEFAULT '',
    agent           TEXT    NOT NULL,
    prompt          TEXT    NOT NULL,
    schedule_kind   TEXT    NOT NULL,
    interval_secs   INTEGER,
    at_ms           INTEGER,
    enabled         INTEGER NOT NULL DEFAULT 1,
    next_run_at_ms  INTEGER,
    last_run_at_ms  INTEGER,
    last_session_id TEXT,
    last_error      TEXT,
    created_at      TEXT    NOT NULL,
    updated_at      TEXT    NOT NULL,
    PRIMARY KEY (user_id, name)
);
INSERT INTO routines_new SELECT '1', * FROM routines;
DROP TABLE routines;
ALTER TABLE routines_new RENAME TO routines;

-- The scheduler is one timer for the deployment, so it scans across accounts;
-- this is the index that scan uses.
CREATE INDEX idx_routines_due ON routines(next_run_at_ms);

-- `environments`
CREATE TABLE environments_new (
    user_id     TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    vendor      TEXT NOT NULL,
    repos       TEXT NOT NULL DEFAULT '[]',
    env_vars    TEXT NOT NULL DEFAULT '[]',
    provision   TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (user_id, name)
);
INSERT INTO environments_new SELECT '1', * FROM environments;
DROP TABLE environments;
ALTER TABLE environments_new RENAME TO environments;

-- `workflows`
CREATE TABLE workflows_new (
    user_id     TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    start       TEXT NOT NULL,
    steps       TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (user_id, name)
);
INSERT INTO workflows_new SELECT '1', * FROM workflows;
DROP TABLE workflows;
ALTER TABLE workflows_new RENAME TO workflows;

-- `provider_oauth`
CREATE TABLE provider_oauth_new (
    user_id    TEXT NOT NULL,
    provider   TEXT NOT NULL,
    access     TEXT NOT NULL,
    refresh    TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    account_id TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, provider)
);
INSERT INTO provider_oauth_new SELECT '1', * FROM provider_oauth;
DROP TABLE provider_oauth;
ALTER TABLE provider_oauth_new RENAME TO provider_oauth;

-- `marketplaces`
CREATE TABLE marketplaces_new (
    user_id       TEXT NOT NULL,
    name          TEXT NOT NULL,
    source_url    TEXT NOT NULL,
    source_ref    TEXT,
    sha           TEXT,
    entries       TEXT NOT NULL,
    skipped       TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    PRIMARY KEY (user_id, name)
);
INSERT INTO marketplaces_new SELECT '1', * FROM marketplaces;
DROP TABLE marketplaces;
ALTER TABLE marketplaces_new RENAME TO marketplaces;

-- `model_cards`. Reference data, but still per-account: an account that cannot
-- add a card for a model nobody blessed cannot use its own self-hosted or
-- newly-released model, which is the opposite of bring-your-own-key.
CREATE TABLE model_cards_new (
    user_id        TEXT NOT NULL,
    model_id       TEXT NOT NULL,
    name           TEXT NOT NULL,
    context_window INTEGER,
    max_tokens     INTEGER,
    created_at     TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at     TEXT NOT NULL DEFAULT (datetime('now')),
    thinking_efforts        TEXT,
    default_thinking_effort TEXT,
    thinking_dialect        TEXT,
    base_url                TEXT,
    forced_tools_disable_thinking INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, model_id)
);
INSERT INTO model_cards_new SELECT '1', * FROM model_cards;
DROP TABLE model_cards;
ALTER TABLE model_cards_new RENAME TO model_cards;

-- `memories`. Rebuilt rather than altered because the UNIQUE constraint has to
-- widen: two accounts may each hold `notes/todo`.
CREATE TABLE memories_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id     TEXT NOT NULL,
    space       TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    UNIQUE (user_id, space, name)
);
INSERT INTO memories_new (id, user_id, space, name, description, content,
                          created_at, updated_at)
SELECT id, '1', space, name, description, content, created_at, updated_at
FROM memories;
DROP TABLE memories;
ALTER TABLE memories_new RENAME TO memories;

CREATE INDEX idx_memories_space ON memories(user_id, space);

-- `github_credentials`. Was a one-row table pinned by `CHECK (id = 1)`; it is
-- now one row *per account*, so the account is the key and the sentinel id
-- goes away.
CREATE TABLE github_credentials_new (
    user_id         TEXT PRIMARY KEY,
    login           TEXT NOT NULL,
    access_token    TEXT NOT NULL,
    refresh_token   TEXT,
    expires_at      TEXT,
    installation_id INTEGER
);
INSERT INTO github_credentials_new (user_id, login, access_token, refresh_token,
                                    expires_at, installation_id)
SELECT '1', login, access_token, refresh_token, expires_at, installation_id
FROM github_credentials;
DROP TABLE github_credentials;
ALTER TABLE github_credentials_new RENAME TO github_credentials;

-- `journal_logs`. Rebuilt because UNIQUE (kind, id) has to widen: two accounts
-- may each run an actor with the same persistence id.
--
-- `journal_events` and `journal_snapshots` reference `journal_logs(log_id)` and
-- are left alone. Their rows survive this: foreign keys are never enabled, so
-- DROP TABLE fires no cascade, and `log_id` values are copied unchanged.
CREATE TABLE journal_logs_new (
    log_id   INTEGER PRIMARY KEY,
    user_id  TEXT    NOT NULL,
    kind     TEXT    NOT NULL,
    id       TEXT    NOT NULL,
    last_seq INTEGER NOT NULL DEFAULT 0,
    UNIQUE (user_id, kind, id)
);
INSERT INTO journal_logs_new (log_id, user_id, kind, id, last_seq)
SELECT log_id, '1', kind, id, last_seq FROM journal_logs;
DROP TABLE journal_logs;
ALTER TABLE journal_logs_new RENAME TO journal_logs;

-- Vestigial: no query site in server/src, and the vendor map is built entirely
-- from agents that dial in. See the comment in config/store.rs.
DROP TABLE vendors;
