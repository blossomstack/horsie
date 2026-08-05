-- PostgreSQL mirror of migrations/sqlite/0022_marketplaces.sql.
--
-- Every column is TEXT, so there is no dialect difference to reconcile here:
-- `entries` and `skipped` are JSON documents this crate parses, not documents
-- the database is asked to index or query into.
CREATE TABLE marketplaces (
    name          TEXT PRIMARY KEY,   -- the index's own `name`, else repo basename
    source_url    TEXT NOT NULL,
    source_ref    TEXT,
    sha           TEXT,               -- HEAD when last read
    entries       TEXT NOT NULL,      -- JSON array of parsed entries
    skipped       TEXT NOT NULL,      -- JSON array of reasons
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

ALTER TABLE plugins ADD COLUMN source_subpath TEXT;
ALTER TABLE plugins ADD COLUMN marketplace TEXT;
ALTER TABLE plugins ADD COLUMN marketplace_entry TEXT;
