-- Put the allocator back where the events are, on a database 0035 truncated.
--
-- 0035 rebuilt `journal_logs` the only way SQLite allows a constraint to change
-- -- create, copy, drop, rename -- on the assumption that foreign keys are never
-- enforced. They were: `sqlx-sqlite` turns them on for every connection unless
-- it is told not to, and `DROP TABLE` performs an implicit `DELETE`, so the
-- rebuild cascaded into `journal_events` and `journal_snapshots` and deleted
-- every event and snapshot in the database. `last_seq` was copied across
-- untouched. `Db::open` now turns the pragma off, which is what stops this
-- happening again; this migration is for the databases it already happened to.
--
-- A log with no events and no snapshot has no history to be behind, so its
-- allocator belongs at zero. Nothing else produces that state: `persist`
-- allocates and inserts in one transaction, compaction only deletes events it
-- has just snapshotted, and `clear` deletes the log row itself.
--
-- The state is not recoverable -- the events are gone. What this restores is a
-- server that runs. Without it every actor recovers at sequence 0, its first
-- write is rejected as a conflict against a `last_seq` nothing can explain, and
-- the write fence stops the actor -- which the API reports on every request as
-- `session supervisor unavailable`.
UPDATE journal_logs SET last_seq = 0
WHERE log_id NOT IN (SELECT log_id FROM journal_events)
  AND log_id NOT IN (SELECT log_id FROM journal_snapshots);
