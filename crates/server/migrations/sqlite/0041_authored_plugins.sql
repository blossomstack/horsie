-- Plugins gain a third kind: authored here, with their source in this database
-- rather than at a git remote. `source_kind` becomes the discriminant of the
-- `PluginKind` union, so the columns only a clone has become nullable and the
-- one only an authored bundle has arrives beside them.
--
-- `artifact_hash` becomes `digest`, because it is no longer the identity of
-- every bundle — only of an external one, which is content-addressed. An
-- authored bundle's identity is its `generation`; the digest is an integrity
-- check on bytes the server re-renders on demand.

CREATE TABLE plugins_new (
    project_id        TEXT NOT NULL,
    name              TEXT NOT NULL,
    -- 'claude' | 'agent_plugin' | 'authored'
    source_kind       TEXT NOT NULL,
    -- External kinds only.
    source_url        TEXT,
    source_ref        TEXT,
    source_subpath    TEXT,
    marketplace       TEXT,
    marketplace_entry TEXT,
    -- Authored only.
    generation        INTEGER,
    version           TEXT,
    description       TEXT,
    catalog           TEXT,
    has_hooks         INTEGER NOT NULL DEFAULT 0,
    digest            TEXT NOT NULL,
    artifact_size     INTEGER NOT NULL,
    enabled_default   INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    PRIMARY KEY (project_id, name)
);

-- Every existing row was installed by a reader that knew only Claude's layout,
-- so that is what they are. `plugin update` re-inspects and may reclassify.
INSERT INTO plugins_new
SELECT project_id, name, 'claude', source_url, source_ref, source_subpath,
       marketplace, marketplace_entry, NULL, version, description, catalog,
       has_hooks, artifact_hash, artifact_size, enabled_default,
       created_at, updated_at
FROM plugins;

DROP TABLE plugins;
ALTER TABLE plugins_new RENAME TO plugins;

-- The authored source of truth. A row here is the editable original; the
-- `plugins` row beside it is the rendered package's metadata.
CREATE TABLE authored_plugins (
    project_id  TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT,
    -- Bumped on every save, and the identity a runtime fetches the package by.
    generation  INTEGER NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (project_id, name)
);

-- The head of each skill. A deleted skill loses its head row and keeps its
-- revisions, so a list is a plain select and the history still answers "what
-- did this say before someone removed it".
CREATE TABLE authored_skills (
    project_id  TEXT NOT NULL,
    plugin      TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL,
    body        TEXT NOT NULL,
    revision    INTEGER NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (project_id, plugin, name)
);

-- The head of each file sitting beside a skill's SKILL.md. `path` is relative
-- to the skill's own directory, so `scripts/run.sh` renders at
-- `skills/<skill>/scripts/run.sh`.
CREATE TABLE authored_skill_files (
    project_id  TEXT NOT NULL,
    plugin      TEXT NOT NULL,
    skill       TEXT NOT NULL,
    path        TEXT NOT NULL,
    content     TEXT NOT NULL,
    PRIMARY KEY (project_id, plugin, skill, path)
);

-- Append-only history. One row per save, including the save that deletes.
CREATE TABLE authored_skill_revisions (
    project_id  TEXT NOT NULL,
    plugin      TEXT NOT NULL,
    skill       TEXT NOT NULL,
    revision    INTEGER NOT NULL,
    description TEXT NOT NULL,
    body        TEXT NOT NULL,
    -- The skill's files at this revision, a JSON array of {path, content}.
    -- One snapshot rather than a fourth history table: a restore has to put
    -- back exactly the set that was there, and the set is what it restores.
    files       TEXT NOT NULL,
    deleted     INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (project_id, plugin, skill, revision)
);
