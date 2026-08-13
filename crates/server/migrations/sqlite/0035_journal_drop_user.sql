-- A journal log is found by `(kind, id)` alone.
--
-- `user_id` was added by 0024 on the reasoning that "two accounts may each run
-- an actor with the same persistence id" -- true in principle, and false here:
-- an account id is random, a session and an agent are uuids. Nothing has ever
-- collided and nothing can.
--
-- What forces it out is that a `Journal` method receives a `PersistenceId` and
-- nothing else, and a persistence id is fixed when its actor is *constructed* --
-- which for a clustered actor is before a single byte of its history has been
-- read. So the account could only reach the journal by being packed into the id,
-- and `Journal` is a framework trait whose users do not all have accounts.
--
-- What the column bought was that another account's log stayed unreachable even
-- given its id. That protection is now that the id is a uuid nothing hands out.
-- Deleting an account's data walks its supervisor's session list.
--
-- Rebuilt rather than `ALTER TABLE ... DROP COLUMN`: the UNIQUE key has to
-- narrow with it, and SQLite cannot drop a column a unique index covers.
-- `journal_events` and `journal_snapshots` reference `journal_logs(log_id)` and
-- are left alone -- foreign keys are never enabled, so DROP TABLE fires no
-- cascade, and `log_id` values are copied unchanged.
CREATE TABLE journal_logs_new (
    log_id   INTEGER PRIMARY KEY,
    kind     TEXT    NOT NULL,
    id       TEXT    NOT NULL,
    last_seq INTEGER NOT NULL DEFAULT 0,
    UNIQUE (kind, id)
);
INSERT INTO journal_logs_new (log_id, kind, id, last_seq)
SELECT log_id, kind, id, last_seq FROM journal_logs;
DROP TABLE journal_logs;
ALTER TABLE journal_logs_new RENAME TO journal_logs;
