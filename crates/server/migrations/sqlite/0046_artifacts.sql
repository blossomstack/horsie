-- Artifacts: the images and documents a message can carry.
--
-- **Why the bytes are here and not in the message.** `AgentState.log` is the
-- whole transcript and it is snapshotted into `journal_snapshots.state` as one
-- blob. An image inlined into a content part would be re-serialized into every
-- future snapshot of that agent, and re-sent on every SSE backfill. A message
-- therefore carries only an id, and the bytes live here once.
--
-- **Why the database and not the disk.** `ArtifactStore` (plugin bundles) writes
-- under `data_dir`, but that is a cache — an authored plugin re-renders from
-- this database. An artifact has no other source of truth, and horsie runs on
-- hosts with ephemeral disks and in multi-node clusters where a local file is
-- not visible to the node that serves the next request.
--
-- **Why metadata and bytes are separate columns rather than separate stores
-- today.** They are split at the type level (`ArtifactBlobs` owns only the
-- bytes) so an object-store backend can be added without touching anything
-- else. Everything except `bytes` is queried and joined, and stays here
-- wherever the bytes end up.
CREATE TABLE artifacts (
    project_id  TEXT    NOT NULL,
    -- Lowercase-hex sha256 of `bytes`. Content-addressed, so re-pasting the
    -- same screenshot stores one row, and `Cache-Control: immutable` on the
    -- fetch route is correct because the URL *is* the hash.
    id          TEXT    NOT NULL,
    -- The *sniffed* type, never the one the client claimed. A browser's
    -- Content-Type is client-controlled and an MCP block's mimeType is whatever
    -- the tool server said; both are claims.
    media_type  TEXT    NOT NULL,
    -- 'image' | 'document'. The discriminant of `ArtifactKind`, stored so a
    -- reader can filter without parsing media types.
    kind        TEXT    NOT NULL,
    byte_size   INTEGER NOT NULL,
    -- Images only, read from the file header rather than by decoding. Null for
    -- a document, and null for an image whose header would not parse.
    width       INTEGER,
    height      INTEGER,
    -- What the client called it. Absent for a paste, which has no filename.
    filename    TEXT,
    bytes       BLOB    NOT NULL,
    created_at  TEXT    NOT NULL,
    PRIMARY KEY (project_id, id)
);

-- Which sessions reference an artifact.
--
-- Content addressing means one row of bytes can be referenced by many sessions,
-- so deleting a session cannot simply delete the artifacts it mentions. A
-- session releases its own rows and the artifact goes only when the last one
-- does. Without this a hosted deployment keeps every image of every deleted
-- session for ever.
CREATE TABLE artifact_uses (
    project_id  TEXT NOT NULL,
    artifact_id TEXT NOT NULL,
    session_id  TEXT NOT NULL,
    PRIMARY KEY (project_id, artifact_id, session_id)
);

-- Release is "delete this session's rows, then delete any artifact with none
-- left", and the second half looks up by artifact rather than by session.
CREATE INDEX artifact_uses_by_session ON artifact_uses (project_id, session_id);
