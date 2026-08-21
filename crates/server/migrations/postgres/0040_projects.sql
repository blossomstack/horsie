-- PostgreSQL mirror of migrations/sqlite/0040_projects.sql.
--
-- Identical statements this time, which is unusual and worth noticing: 0024
-- needed sixteen table rebuilds on SQLite and sixteen four-statement ALTERs
-- here because it was widening primary keys. This only renames a column, and
-- both engines rewrite the PRIMARY KEY, the UNIQUE constraints and every index
-- definition that names it. See the SQLite file for why the id is copied.
--
-- `is_default` stays INTEGER rather than BOOLEAN, as `enabled_default` and
-- `enabled` already do: the `Any` driver decodes a PostgreSQL BOOLEAN
-- differently from a SQLite integer, and one storage shape for both dialects is
-- what keeps a single row-mapping function honest.

CREATE TABLE projects (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL,
    name       TEXT NOT NULL,
    is_default INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (user_id, name)
);

CREATE INDEX idx_projects_user ON projects (user_id);

INSERT INTO projects (id, user_id, name, is_default, created_at, updated_at)
SELECT owner,
       owner,
       'Default',
       1,
       to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS'),
       to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')
FROM (
    SELECT id      AS owner FROM auth_users
    UNION SELECT user_id FROM providers
    UNION SELECT user_id FROM models
    UNION SELECT user_id FROM settings
    UNION SELECT user_id FROM mcp_servers
    UNION SELECT user_id FROM plugins
    UNION SELECT user_id FROM memory_spaces
    UNION SELECT user_id FROM memories
    UNION SELECT user_id FROM agents
    UNION SELECT user_id FROM routines
    UNION SELECT user_id FROM environments
    UNION SELECT user_id FROM workflows
    UNION SELECT user_id FROM provider_oauth
    UNION SELECT user_id FROM marketplaces
    UNION SELECT user_id FROM model_cards
    UNION SELECT user_id FROM github_credentials
    UNION SELECT user_id FROM runtime_vendors
) AS owners;

ALTER TABLE providers          RENAME COLUMN user_id TO project_id;
ALTER TABLE models             RENAME COLUMN user_id TO project_id;
ALTER TABLE settings           RENAME COLUMN user_id TO project_id;
ALTER TABLE mcp_servers        RENAME COLUMN user_id TO project_id;
ALTER TABLE plugins            RENAME COLUMN user_id TO project_id;
ALTER TABLE memory_spaces      RENAME COLUMN user_id TO project_id;
ALTER TABLE memories           RENAME COLUMN user_id TO project_id;
ALTER TABLE agents             RENAME COLUMN user_id TO project_id;
ALTER TABLE routines           RENAME COLUMN user_id TO project_id;
ALTER TABLE environments       RENAME COLUMN user_id TO project_id;
ALTER TABLE workflows          RENAME COLUMN user_id TO project_id;
ALTER TABLE provider_oauth     RENAME COLUMN user_id TO project_id;
ALTER TABLE marketplaces       RENAME COLUMN user_id TO project_id;
ALTER TABLE model_cards        RENAME COLUMN user_id TO project_id;
ALTER TABLE github_credentials RENAME COLUMN user_id TO project_id;
ALTER TABLE runtime_vendors    RENAME COLUMN user_id TO project_id;
