-- Agent-managed long-term memories, grouped into named spaces. Sessions select
-- spaces at creation; the agent sees an index of the memories in the selected
-- spaces and loads bodies on demand with the memory_load tool.
--
-- `memories.space` is deliberately NOT declared as a SQL foreign key: no table
-- in this schema uses REFERENCES and `PRAGMA foreign_keys` is never enabled in
-- `open_pool`, so a declared ON DELETE CASCADE would be silently ignored --
-- worse than no constraint at all. MemoryStore enforces the relationship in
-- explicit transactions instead (delete_space, rename_space, create_memory).

CREATE TABLE memory_spaces (
    name        TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL,              -- unix epoch seconds
    updated_at  TEXT NOT NULL               -- unix epoch seconds
);

INSERT INTO memory_spaces (name, description, created_at, updated_at)
    VALUES ('default', 'Default memory space', strftime('%s','now'), strftime('%s','now'));

CREATE TABLE memories (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    space       TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    UNIQUE (space, name)
);

CREATE INDEX idx_memories_space ON memories(space);
