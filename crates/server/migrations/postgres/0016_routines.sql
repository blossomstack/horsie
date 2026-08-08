-- PostgreSQL mirror of migrations/sqlite/0016_routines.sql.
--
-- Routines: an agent preset (by name) plus a fixed prompt and a trigger.
-- Running one creates an unattended session tagged with the routine's name in
-- its spec; those sessions are listed under the routine, not in the session
-- list. `agent` is deliberately not a SQL foreign key — the reference is
-- validated by the service at save and again at run, which is where a useful
-- error message can be produced.

CREATE TABLE routines (
    name            TEXT PRIMARY KEY,
    description     TEXT    NOT NULL DEFAULT '',
    agent           TEXT    NOT NULL,
    prompt          TEXT    NOT NULL,
    schedule_kind   TEXT    NOT NULL,            -- manual | every | once
    interval_secs   BIGINT,                      -- non-NULL iff 'every'
    at_ms           BIGINT,                      -- non-NULL iff 'once'
    enabled         INTEGER NOT NULL DEFAULT 1,  -- 0 pauses the timer only
    next_run_at_ms  BIGINT,                      -- NULL → nothing scheduled
    last_run_at_ms  BIGINT,
    last_session_id TEXT,
    last_error      TEXT,
    created_at      TEXT    NOT NULL,            -- unix epoch seconds
    updated_at      TEXT    NOT NULL             -- unix epoch seconds
);

-- The scheduler's only query: enabled routines whose next run is due.
CREATE INDEX routines_next_run ON routines (next_run_at_ms) WHERE enabled = 1;
