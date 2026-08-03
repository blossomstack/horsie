-- PostgreSQL mirror of migrations/sqlite/0016_journal.sql.
--
-- The actor journal: every session's and agent's event log, plus the latest
-- snapshot of each. Used when `journal.backend` is `database`; the alternative
-- is the JSONL-file journal the CLI still uses.
--
-- Sequence numbers are 1-based and monotonic per (actor_kind, actor_id), and an
-- event's number is stable for the life of the log even after compaction drops
-- older events — that is the `Journal` trait's contract, not an implementation
-- detail, so the sequence is stored rather than derived from row order.
--
-- The composite primary key is the only index either table needs. Every access
-- path is (actor_kind, actor_id) equality plus a `seq` range or ordering, which
-- the primary key serves as a left-prefix scan: no sort, and nothing extra to
-- maintain on the write path. Payloads above ~2 KB move to TOAST storage, which
-- is the behaviour we want: the index stays dense and only the rows actually
-- replayed pay to decompress.
--
-- It is also the correctness backstop for the single-writer assumption this
-- design rests on: one process owns each persistence id, so a duplicate
-- sequence number means that assumption broke, and it surfaces as a constraint
-- error instead of silently interleaving two actors' events.

CREATE TABLE journal_events (
    actor_kind TEXT   NOT NULL,
    actor_id   TEXT   NOT NULL,
    seq        BIGINT NOT NULL,
    payload    BYTEA  NOT NULL,
    PRIMARY KEY (actor_kind, actor_id, seq)
);

CREATE TABLE journal_snapshots (
    actor_kind TEXT   NOT NULL,
    actor_id   TEXT   NOT NULL,
    seq        BIGINT NOT NULL,   -- sequence of the last event folded into `state`
    state      BYTEA  NOT NULL,
    PRIMARY KEY (actor_kind, actor_id)
);
