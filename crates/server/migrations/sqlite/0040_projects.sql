-- Every durable row belongs to a project, and every project belongs to a user.
--
-- 0024 made the scope the account. This splits the account into one or more
-- projects and moves the scope down a level: a user's providers, models,
-- skills, agents, memories and sessions now belong to one project and are
-- invisible from the next. Nothing is shared between projects, deliberately --
-- a new project starts empty, credentials included.
--
-- `user_id` survives only where it always meant *identity* rather than scope:
-- `auth_users`, `auth_tokens`, `auth_device_codes`, and `projects.user_id`.
--
-- A RENAME, not a rebuild. 0024 rebuilt sixteen tables because it was widening
-- primary keys, which SQLite cannot ALTER; renaming a column it can, and it
-- rewrites the PRIMARY KEY, the UNIQUE constraints and every index definition
-- that names the column along with it. Verified against a migrated database
-- rather than assumed: `idx_memories_space` comes out as
-- `(project_id, space)`. So none of 0024's index-loss hazard applies here, and
-- the two dialects stay identical because both are doing the same ALTER.
--
-- Existing accounts get a default project whose **id is the user's id**. That
-- is a one-time device with two purposes, and it is not a rule the code
-- follows -- `ProjectId::generate` is the only way an id is minted from Rust,
-- for the default project included.
--
--   1. The scope column is copied, never rewritten, so SQL and Rust have no id
--      to disagree about. 0024 shipped a backfill that hardcoded '1' while
--      Rust minted a random id, and every deployment upgraded from
--      `auth.enabled = false` came up healthy and empty.
--   2. An actor's persistence id and a bus topic both embed the scope string.
--      Copying the id renders every existing address byte-identically, so live
--      sessions, their journals and their session-supervisor logs survive this
--      migration untouched. Minting a fresh id here would have orphaned them.
--
-- The seed reads the union of `auth_users` and every scoped table rather than
-- `auth_users` alone. A deployment running with authentication off has rows
-- owned by an account that has no `auth_users` row at all -- exactly the case
-- 0024 lost -- and one running with it on has an account per row owner anyway,
-- so the union is a superset of both and costs one pass over sixteen small
-- indexes.

CREATE TABLE projects (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL,
    name       TEXT NOT NULL,
    -- Exactly one per user, and it cannot be deleted. Not a partial unique
    -- index: SQLite and PostgreSQL spell that differently, and the invariant is
    -- already enforced where projects are created and deleted.
    is_default INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (user_id, name)
);

CREATE INDEX idx_projects_user ON projects (user_id);

INSERT INTO projects (id, user_id, name, is_default, created_at, updated_at)
SELECT owner, owner, 'Default', 1, datetime('now'), datetime('now')
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
);

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
