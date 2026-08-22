-- See the sqlite copy for why. Postgres can relax the constraint and rename the
-- column in place, so there is no table rebuild here.

ALTER TABLE plugins RENAME COLUMN artifact_hash TO digest;
ALTER TABLE plugins ALTER COLUMN source_url DROP NOT NULL;
ALTER TABLE plugins ADD COLUMN generation BIGINT;

-- Every existing row was installed by a reader that knew only Claude's layout.
UPDATE plugins SET source_kind = 'claude' WHERE source_kind = 'git';

CREATE TABLE authored_plugins (
    project_id  TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT,
    generation  BIGINT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (project_id, name)
);

CREATE TABLE authored_skills (
    project_id  TEXT NOT NULL,
    plugin      TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL,
    body        TEXT NOT NULL,
    revision    BIGINT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (project_id, plugin, name)
);

CREATE TABLE authored_skill_files (
    project_id  TEXT NOT NULL,
    plugin      TEXT NOT NULL,
    skill       TEXT NOT NULL,
    path        TEXT NOT NULL,
    content     TEXT NOT NULL,
    PRIMARY KEY (project_id, plugin, skill, path)
);

CREATE TABLE authored_skill_revisions (
    project_id  TEXT NOT NULL,
    plugin      TEXT NOT NULL,
    skill       TEXT NOT NULL,
    revision    BIGINT NOT NULL,
    description TEXT NOT NULL,
    body        TEXT NOT NULL,
    files       TEXT NOT NULL,
    -- INTEGER, not BOOLEAN: every other flag in this schema is bound and read
    -- as an i64 through sqlx::Any, and a lone BOOLEAN column reads as a type
    -- mismatch on one driver only.
    deleted     INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (project_id, plugin, skill, revision)
);
