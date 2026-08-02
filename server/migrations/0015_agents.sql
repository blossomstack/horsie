-- Named agent presets: a saved session configuration (vendor, model, repos,
-- skills, MCP servers, memory spaces, thinking effort) invoked with a message
-- to create a session. List-typed columns are JSON arrays; `repos` elements
-- are {"url", "git_ref"?, "dir"?}.

CREATE TABLE agents (
    name            TEXT PRIMARY KEY,
    description     TEXT NOT NULL DEFAULT '',
    vendor          TEXT,                       -- NULL → server default at invoke
    model           TEXT NOT NULL,
    repos           TEXT NOT NULL DEFAULT '[]',
    plugins         TEXT NOT NULL DEFAULT '[]',
    mcp_servers     TEXT NOT NULL DEFAULT '[]',
    memory_spaces   TEXT NOT NULL DEFAULT '[]',
    thinking_effort TEXT,
    created_at      TEXT NOT NULL,              -- unix epoch seconds
    updated_at      TEXT NOT NULL               -- unix epoch seconds
);
