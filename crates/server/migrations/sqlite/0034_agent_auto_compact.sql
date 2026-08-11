-- Whether sessions from a preset compact automatically once their context
-- fills. NULL means yes, so every preset that predates compaction gains the
-- behaviour without a backfill — the flag exists to turn it *off*.
--
-- INTEGER, not BOOLEAN: the `sqlx::Any` driver cannot decode SQLite's BOOLEAN,
-- and every other flag in this schema is stored the same way for the same
-- reason.
ALTER TABLE agents ADD COLUMN auto_compact INTEGER;
