-- See the sqlite copy for why this table exists and why both entity kinds
-- share it. BIGINT rather than INTEGER for the counters, as elsewhere.
CREATE TABLE entity_revisions (
    project_id  TEXT   NOT NULL,
    entity_kind TEXT   NOT NULL,
    entity_id   TEXT   NOT NULL,
    revision    BIGINT NOT NULL,
    payload     TEXT   NOT NULL,
    -- INTEGER, not BOOLEAN: every other flag in this schema is bound and read
    -- as an i64 through sqlx::Any, and a lone BOOLEAN column reads as a type
    -- mismatch on one driver only.
    deleted     INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT   NOT NULL,
    PRIMARY KEY (project_id, entity_kind, entity_id, revision)
);

ALTER TABLE agents ADD COLUMN revision BIGINT;
ALTER TABLE memories ADD COLUMN revision BIGINT;
