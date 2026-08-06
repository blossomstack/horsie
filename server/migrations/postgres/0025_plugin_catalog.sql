-- What each bundle offers — its commands, skills and agents — derived once at
-- ingest so nothing downstream has to re-scan a checkout to find out. NULL on
-- rows installed before this shipped; the service backfills them on read from
-- the artifact zip, which is still on disk.
ALTER TABLE plugins ADD COLUMN catalog TEXT;

-- Derivable from `catalog`, and two sources for one fact is how they drift.
ALTER TABLE plugins DROP COLUMN skill_count;
