CREATE TABLE plugins (
    name            TEXT PRIMARY KEY,
    source_kind     TEXT NOT NULL,            -- 'git'
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
    updated_at      TEXT NOT NULL
);
