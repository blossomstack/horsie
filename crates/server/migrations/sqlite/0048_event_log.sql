-- A durable, at-least-once event log with per-consumer-group cursors.
--
-- SQLite needs no visibility gate. One writer at a time, and `begin_write`
-- takes the lock up front with BEGIN IMMEDIATE, so writers serialise on commit
-- and insert order *is* commit order. A reader may therefore trust `id`
-- directly. The PostgreSQL migration carries an `xid8` column for exactly the
-- guarantee this dialect gets for free.
CREATE TABLE event_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id TEXT NOT NULL,
    stream TEXT NOT NULL,
    -- What both sides compute from the event itself, so appending the same
    -- event twice is a no-op rather than a duplicate delivery.
    event_id TEXT NOT NULL,
    payload BLOB NOT NULL,
    created_at INTEGER NOT NULL
);

-- Dedup on production. An append that loses a race with itself — an actor
-- retrying after a crash, a request sent twice — conflicts here and is dropped.
CREATE UNIQUE INDEX event_log_dedup ON event_log (project_id, stream, event_id);

-- The read path: one consumer group walking a stream in id order.
CREATE INDEX event_log_read ON event_log (project_id, stream, id);

-- Where each consumer group has got to. A row appears on first ack; a group
-- with no row starts from the beginning of what the log still holds.
CREATE TABLE event_log_offsets (
    project_id TEXT NOT NULL,
    stream TEXT NOT NULL,
    consumer_group TEXT NOT NULL,
    acked_id INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, stream, consumer_group)
);
