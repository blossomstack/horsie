-- PostgreSQL mirror of migrations/sqlite/0009_memory.sql.
--
-- Agent-managed long-term memories, grouped into named spaces. Sessions select
-- spaces at creation; the agent sees an index of the memories in the selected
-- spaces and loads bodies on demand with the memory_load tool.
--
-- `memories.space` is deliberately NOT declared as a SQL foreign key. On SQLite
-- `PRAGMA foreign_keys` is never enabled, so a declared ON DELETE CASCADE would
-- be silently ignored -- worse than no constraint at all. PostgreSQL would
-- enforce it, and the two backends enforcing different invariants is a worse
-- bug than the one it fixes, so the constraint is omitted here too.
-- MemoryStore enforces the relationship in explicit transactions instead
-- (delete_space, rename_space, create_memory).
--
-- `id` is BIGSERIAL where SQLite has INTEGER PRIMARY KEY AUTOINCREMENT. Inserts
-- read the assigned id back with RETURNING rather than last_insert_rowid(),
-- which sqlx's Any driver reports as NULL on SQLite regardless.

CREATE TABLE memory_spaces (
    name        TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL,              -- unix epoch seconds
    updated_at  TEXT NOT NULL               -- unix epoch seconds
);

INSERT INTO memory_spaces (name, description, created_at, updated_at)
    VALUES ('default', 'Default memory space',
            EXTRACT(EPOCH FROM now())::bigint::text,
            EXTRACT(EPOCH FROM now())::bigint::text);

CREATE TABLE memories (
    id          BIGSERIAL PRIMARY KEY,
    space       TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    UNIQUE (space, name)
);

CREATE INDEX idx_memories_space ON memories(space);
