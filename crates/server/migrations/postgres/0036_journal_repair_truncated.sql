-- The SQLite arm of this migration repairs logs whose events 0035 deleted; see
-- the comment there for how that happened.
--
-- Nothing to repair on PostgreSQL: it drops the column in place, so there was no
-- table drop to cascade from. Present because the two directories declare the
-- same versions, and because a database that was dumped out of SQLite and
-- restored here would arrive with the same damage.
UPDATE journal_logs SET last_seq = 0
WHERE log_id NOT IN (SELECT log_id FROM journal_events)
  AND log_id NOT IN (SELECT log_id FROM journal_snapshots);
