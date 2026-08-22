-- Version history and compare-and-set for the two things a tuning agent
-- rewrites: agent presets, and the memories an agent has accumulated.
--
-- Both gain the same two properties, so they get one table rather than two.
-- `authored_skills` already has its own history and keeps it — the shape here
-- is deliberately compatible so it could move later, but moving it now would be
-- churn in service of symmetry.
--
-- **Why history.** A tuning agent rewrites an agent's instructions from its own
-- past runs. That is a judgement call made without a human in the loop, on a
-- schedule, and the failure mode is a preset that got quietly worse three weeks
-- ago. Being able to read what it used to say, and put it back, is what makes
-- letting it write at all a reasonable trade.
--
-- **Why compare-and-set.** `PUT /agents/{name}` is a full replace. A tuner that
-- reads a preset, thinks, and writes it back has a window in which a person can
-- edit the same preset — and the tuner's write silently reverts them. There is
-- no merge that would be right here, so the write is refused instead and the
-- caller re-reads.
--
-- One row per save, including the save that deletes. `payload` is a JSON
-- snapshot of the whole entity rather than a diff: a restore has to put back
-- exactly what was there, and reconstructing that from diffs means every
-- reader of this table has to agree on how they compose.
CREATE TABLE entity_revisions (
    project_id  TEXT    NOT NULL,
    -- 'agent' | 'memory'. Not a foreign key: an entity's history outlives the
    -- entity, which is the point of keeping the row that recorded its deletion.
    entity_kind TEXT    NOT NULL,
    -- The entity's own id of record: a preset's name, a memory's numeric id as
    -- text. Text for both, because the two kinds are keyed differently and a
    -- shared table cannot be both.
    entity_id   TEXT    NOT NULL,
    -- 1 for the first save, and never reused. A restore appends a new
    -- revision rather than rewinding the counter: history is what it is for,
    -- and rewinding would lose the fact that a restore happened.
    revision    INTEGER NOT NULL,
    payload     TEXT    NOT NULL,
    -- INTEGER, not BOOLEAN: the `sqlx::Any` driver cannot decode SQLite's
    -- BOOLEAN, and every other flag in this schema is stored the same way.
    deleted     INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT    NOT NULL,
    PRIMARY KEY (project_id, entity_kind, entity_id, revision)
);

-- The head revision each entity is currently at, so a caller can compare
-- against it without reading the history.
--
-- NULL on every existing row, which reads as "never versioned". A write that
-- names an expected revision against a NULL head is refused: the caller is
-- claiming to know a version this row has never had.
ALTER TABLE agents ADD COLUMN revision INTEGER;
ALTER TABLE memories ADD COLUMN revision INTEGER;
