-- See the sqlite copy for why this table exists and why each column is shaped
-- the way it is. BIGINT rather than INTEGER: epoch-ms timestamps overflow a
-- 32-bit int, and Postgres's INTEGER really is 32 bits where SQLite's is not.
CREATE TABLE inbox_messages (
    project_id   TEXT   NOT NULL,
    id           TEXT   NOT NULL,
    kind         TEXT   NOT NULL,
    state        TEXT   NOT NULL,
    session_id   TEXT   NOT NULL,
    agent_id     TEXT   NOT NULL,
    title        TEXT   NOT NULL,
    body         TEXT   NOT NULL,
    payload      TEXT   NOT NULL,
    tool_call_id TEXT,
    created_at   BIGINT NOT NULL,
    read_at      BIGINT,
    resolved_at  BIGINT,
    PRIMARY KEY (project_id, id)
);

CREATE UNIQUE INDEX inbox_messages_ask_call
    ON inbox_messages (project_id, session_id, agent_id, tool_call_id)
    WHERE tool_call_id IS NOT NULL;

CREATE INDEX inbox_messages_recent ON inbox_messages (project_id, created_at DESC);

CREATE INDEX inbox_messages_by_agent ON inbox_messages (project_id, session_id, agent_id);
