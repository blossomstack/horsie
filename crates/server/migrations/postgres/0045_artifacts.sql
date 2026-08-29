-- See the sqlite copy for why artifacts are stored here rather than inside a
-- message, and why a use table is needed to delete them.
--
-- BYTEA rather than BLOB, and BIGINT rather than INTEGER for the counters, as
-- elsewhere in this schema.
CREATE TABLE artifacts (
    project_id  TEXT   NOT NULL,
    id          TEXT   NOT NULL,
    media_type  TEXT   NOT NULL,
    kind        TEXT   NOT NULL,
    byte_size   BIGINT NOT NULL,
    width       BIGINT,
    height      BIGINT,
    filename    TEXT,
    bytes       BYTEA  NOT NULL,
    created_at  TEXT   NOT NULL,
    PRIMARY KEY (project_id, id)
);

CREATE TABLE artifact_uses (
    project_id  TEXT NOT NULL,
    artifact_id TEXT NOT NULL,
    session_id  TEXT NOT NULL,
    PRIMARY KEY (project_id, artifact_id, session_id)
);

CREATE INDEX artifact_uses_by_session ON artifact_uses (project_id, session_id);
