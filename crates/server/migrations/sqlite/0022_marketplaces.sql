-- Registered marketplaces: a git source plus the catalogue it last offered.
--
-- `entries` is the PARSED index, cached. The official marketplace has ~276
-- entries and browsing it is a page render, so a git clone must not sit on that
-- path. The cost is that the cache is a snapshot: a plugin published since the
-- last read appears only after POST /api/marketplaces/:name/refresh.
--
-- Storing the parsed form rather than the raw file means a marketplace.json
-- schema change is absorbed at refresh time by the same parser the CLI uses,
-- rather than at read time by a second one.
--
-- No `plugin_count` column: it is the length of `entries`, and a denormalised
-- count that can disagree with the column beside it is a bug waiting to happen.
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

-- Where inside its checkout a bundle's plugin root sat, so `update` re-clones
-- the same tree rather than the repo root. NULL for a plain bundle repo, which
-- is every row that exists today.
ALTER TABLE plugins ADD COLUMN source_subpath TEXT;

-- Provenance for a bundle installed through a marketplace. Both columns or
-- neither. The index's name for an entry is not always the name it installs as
-- (`42crunch-api-security-testing` installs as `api-security-testing`), which
-- is why both the marketplace and the entry are recorded.
ALTER TABLE plugins ADD COLUMN marketplace TEXT;
ALTER TABLE plugins ADD COLUMN marketplace_entry TEXT;
