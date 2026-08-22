-- See the sqlite copy for why this table exists and why it is this narrow.
-- BIGINT rather than INTEGER: epoch-ms timestamps overflow a 32-bit int, and
-- Postgres's INTEGER really is 32 bits where SQLite's is not.
CREATE TABLE agent_runs (
    project_id TEXT   NOT NULL,
    session_id TEXT   NOT NULL,
    agent_id   TEXT   NOT NULL,
    preset     TEXT,
    status     TEXT   NOT NULL,
    started_at BIGINT NOT NULL,
    ended_at   BIGINT,
    PRIMARY KEY (project_id, session_id, agent_id)
);

CREATE INDEX agent_runs_by_preset ON agent_runs (project_id, preset, started_at DESC);
