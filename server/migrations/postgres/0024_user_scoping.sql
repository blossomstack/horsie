-- PostgreSQL mirror of migrations/sqlite/0024_user_scoping.sql.
--
-- Same shape, without the rebuilds: PostgreSQL can alter a primary key, a
-- UNIQUE constraint and a column type in place.
--
-- The DROP DEFAULT after each backfill is load-bearing and is why every table
-- gets four statements rather than two. A lingering DEFAULT '1' would make an
-- INSERT that forgets the scope land silently in the bootstrap account's data;
-- without one it is a NOT NULL violation. See the SQLite file for the rest.
--
-- Constraint names are PostgreSQL's defaults for constraints declared inline:
-- `<table>_pkey` for a primary key and `<table>_<columns>_key` for a UNIQUE.

-- `providers`
ALTER TABLE providers ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE providers DROP CONSTRAINT providers_pkey;
ALTER TABLE providers ADD PRIMARY KEY (user_id, name);
ALTER TABLE providers ALTER COLUMN user_id DROP DEFAULT;

-- `models`
ALTER TABLE models ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE models DROP CONSTRAINT models_pkey;
ALTER TABLE models ADD PRIMARY KEY (user_id, alias);
ALTER TABLE models ALTER COLUMN user_id DROP DEFAULT;

-- `settings`
ALTER TABLE settings ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE settings DROP CONSTRAINT settings_pkey;
ALTER TABLE settings ADD PRIMARY KEY (user_id, key);
ALTER TABLE settings ALTER COLUMN user_id DROP DEFAULT;

-- `mcp_servers`
ALTER TABLE mcp_servers ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE mcp_servers DROP CONSTRAINT mcp_servers_pkey;
ALTER TABLE mcp_servers ADD PRIMARY KEY (user_id, name);
ALTER TABLE mcp_servers ALTER COLUMN user_id DROP DEFAULT;

-- `plugins`
ALTER TABLE plugins ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE plugins DROP CONSTRAINT plugins_pkey;
ALTER TABLE plugins ADD PRIMARY KEY (user_id, name);
ALTER TABLE plugins ALTER COLUMN user_id DROP DEFAULT;

-- `memory_spaces`
ALTER TABLE memory_spaces ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE memory_spaces DROP CONSTRAINT memory_spaces_pkey;
ALTER TABLE memory_spaces ADD PRIMARY KEY (user_id, name);
ALTER TABLE memory_spaces ALTER COLUMN user_id DROP DEFAULT;

-- `agents`
ALTER TABLE agents ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE agents DROP CONSTRAINT agents_pkey;
ALTER TABLE agents ADD PRIMARY KEY (user_id, name);
ALTER TABLE agents ALTER COLUMN user_id DROP DEFAULT;

-- `routines`
ALTER TABLE routines ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE routines DROP CONSTRAINT routines_pkey;
ALTER TABLE routines ADD PRIMARY KEY (user_id, name);
ALTER TABLE routines ALTER COLUMN user_id DROP DEFAULT;

-- The scheduler is one timer for the deployment, so it scans across accounts;
-- this is the index that scan uses.
CREATE INDEX idx_routines_due ON routines(next_run_at_ms);

-- `environments`
ALTER TABLE environments ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE environments DROP CONSTRAINT environments_pkey;
ALTER TABLE environments ADD PRIMARY KEY (user_id, name);
ALTER TABLE environments ALTER COLUMN user_id DROP DEFAULT;

-- `workflows`
ALTER TABLE workflows ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE workflows DROP CONSTRAINT workflows_pkey;
ALTER TABLE workflows ADD PRIMARY KEY (user_id, name);
ALTER TABLE workflows ALTER COLUMN user_id DROP DEFAULT;

-- `provider_oauth`
ALTER TABLE provider_oauth ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE provider_oauth DROP CONSTRAINT provider_oauth_pkey;
ALTER TABLE provider_oauth ADD PRIMARY KEY (user_id, provider);
ALTER TABLE provider_oauth ALTER COLUMN user_id DROP DEFAULT;

-- `marketplaces`
ALTER TABLE marketplaces ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE marketplaces DROP CONSTRAINT marketplaces_pkey;
ALTER TABLE marketplaces ADD PRIMARY KEY (user_id, name);
ALTER TABLE marketplaces ALTER COLUMN user_id DROP DEFAULT;

-- `model_cards`. Reference data, but still per-account: an account that cannot
-- add a card for a model nobody blessed cannot use its own self-hosted or
-- newly-released model, which is the opposite of bring-your-own-key.
ALTER TABLE model_cards ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE model_cards DROP CONSTRAINT model_cards_pkey;
ALTER TABLE model_cards ADD PRIMARY KEY (user_id, model_id);
ALTER TABLE model_cards ALTER COLUMN user_id DROP DEFAULT;

-- `memories`. The UNIQUE has to widen: two accounts may each hold `notes/todo`.
ALTER TABLE memories ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE memories DROP CONSTRAINT memories_space_name_key;
ALTER TABLE memories ADD UNIQUE (user_id, space, name);
ALTER TABLE memories ALTER COLUMN user_id DROP DEFAULT;

CREATE INDEX idx_memories_space ON memories(user_id, space);

-- `github_credentials`. Was a one-row table pinned by `CHECK (id = 1)`; it is
-- now one row *per account*, so the account is the key. Dropping the `id`
-- column takes its CHECK constraint with it.
ALTER TABLE github_credentials ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE github_credentials DROP CONSTRAINT github_credentials_pkey;
ALTER TABLE github_credentials DROP COLUMN id;
ALTER TABLE github_credentials ADD PRIMARY KEY (user_id);
ALTER TABLE github_credentials ALTER COLUMN user_id DROP DEFAULT;

-- `journal_logs`. UNIQUE (kind, id) has to widen: two accounts may each run an
-- actor with the same persistence id. `journal_events` and `journal_snapshots`
-- are untouched -- they are reached only by `log_id`, which comes from the
-- scoped lookup here.
ALTER TABLE journal_logs ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE journal_logs DROP CONSTRAINT journal_logs_kind_id_key;
ALTER TABLE journal_logs ADD UNIQUE (user_id, kind, id);
ALTER TABLE journal_logs ALTER COLUMN user_id DROP DEFAULT;

-- Vestigial: no query site in server/src, and the vendor map is built entirely
-- from agents that dial in. See the comment in config/store.rs.
DROP TABLE vendors;
