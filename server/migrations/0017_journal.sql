-- Event-sourced actor journal.
--
-- Shares this database with the settings store, so the schema lives in the
-- server's migration chain: two sqlx migrators on one database would collide on
-- `_sqlx_migrations`.

-- One row per persistent actor. `last_seq` lives here rather than being derived
-- as MAX(seq) over the events, and that is the whole point: compaction deletes
-- events, so a derived number would restart after one and silently renumber the
-- log. Keeping it on the log row is what makes an event's sequence number stable
-- for the life of the log, which is what the `Journal` trait promises.
CREATE TABLE journal_logs (
    log_id   INTEGER PRIMARY KEY,
    kind     TEXT    NOT NULL,
    id       TEXT    NOT NULL,
    last_seq INTEGER NOT NULL DEFAULT 0,
    UNIQUE (kind, id)
);

-- WITHOUT ROWID makes the primary key the storage order, so the one query that
-- matters -- WHERE log_id = ? AND seq > ? ORDER BY seq -- is a contiguous range
-- scan with no secondary-index indirection and no row-payload hop.
--
-- `payload` is the caller's opaque bytes, stored raw: unlike the file backend
-- there is no base64 layer, so no encode/decode per event and ~25% less space.
CREATE TABLE journal_events (
    log_id  INTEGER NOT NULL REFERENCES journal_logs(log_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    payload BLOB    NOT NULL,
    PRIMARY KEY (log_id, seq)
) WITHOUT ROWID;

-- At most one snapshot per log; saving replaces it. `seq` is the sequence number
-- of the last event folded into `state`, so recovery replays strictly after it.
CREATE TABLE journal_snapshots (
    log_id INTEGER PRIMARY KEY REFERENCES journal_logs(log_id) ON DELETE CASCADE,
    seq    INTEGER NOT NULL,
    state  BLOB    NOT NULL
);
