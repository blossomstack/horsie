-- A durable, at-least-once event log with per-consumer-group cursors.
--
-- The `xid` column is why this dialect's schema differs from SQLite's.
-- `BIGSERIAL` is allocated *before* commit, so two writers can commit out of
-- sequence order: a reader polling `id > last_seen` can see 13, record it, and
-- permanently skip 12 when that transaction lands a moment later. Silent,
-- unrecoverable loss on a log that claims at-least-once.
--
-- Transaction ids do not have that flaw — they are monotonic with respect to
-- commit — so a reader gates on `xid < pg_snapshot_xmin(pg_current_snapshot())`
-- and reads only rows no in-flight transaction can still land beneath. The cost
-- is that a long-running write transaction delays every delivery until it
-- commits, which is acceptable here only because this server's write
-- transactions are short row writes with no I/O held open across them.
CREATE TABLE event_log (
    id BIGSERIAL PRIMARY KEY,
    project_id TEXT NOT NULL,
    stream TEXT NOT NULL,
    -- What both sides compute from the event itself, so appending the same
    -- event twice is a no-op rather than a duplicate delivery.
    event_id TEXT NOT NULL,
    payload BYTEA NOT NULL,
    created_at BIGINT NOT NULL,
    -- Never read into Rust: `sqlx::Any` has no mapping for `xid8`. It is
    -- written by the default and compared only inside SQL.
    xid xid8 NOT NULL DEFAULT pg_current_xact_id()
);

-- Dedup on production. An append that loses a race with itself — an actor
-- retrying after a crash, a request sent twice — conflicts here and is dropped.
CREATE UNIQUE INDEX event_log_dedup ON event_log (project_id, stream, event_id);

-- The read path: one consumer group walking a stream in id order, gated on the
-- transaction id. Both columns are in the index because both are in the WHERE.
CREATE INDEX event_log_read ON event_log (project_id, stream, id, xid);

-- Where each consumer group has got to. A row appears on first ack; a group
-- with no row starts from the beginning of what the log still holds.
CREATE TABLE event_log_offsets (
    project_id TEXT NOT NULL,
    stream TEXT NOT NULL,
    consumer_group TEXT NOT NULL,
    acked_id BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (project_id, stream, consumer_group)
);
