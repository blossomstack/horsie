-- PostgreSQL mirror of migrations/sqlite/0019_environments.sql.
--
-- Named environments (experimental): a reusable runtime + repos bundle. Nothing
-- references one yet — this is the first step of the environments exploration.
-- List-typed columns are JSON arrays; `repos` elements are
-- {"url", "git_ref"?, "dir"?}, `env_vars` are {"name", "value"}, `provision`
-- elements are {"name", "uses", "with": [{"key", "value"}]}.

CREATE TABLE environments (
    name        TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    vendor      TEXT NOT NULL,
    repos       TEXT NOT NULL DEFAULT '[]',
    env_vars    TEXT NOT NULL DEFAULT '[]',
    provision   TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,              -- unix epoch seconds
    updated_at  TEXT NOT NULL               -- unix epoch seconds
);
