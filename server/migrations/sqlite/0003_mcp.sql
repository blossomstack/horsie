-- Configured remote MCP servers the session agent may call server-side. One
-- row per server, keyed by `name` (also the tool namespace `mcp__<name>__…`).
-- `auth_kind` tags how horsie authenticates: `none` (public), `bearer` (static
-- token stored here), or `github_app` (a user token minted from the GitHub App
-- connection at use time — nothing stored on the row). Secrets are stored
-- plaintext like provider api_keys (the DB file is the trust boundary); views
-- redact them to a `has_token` flag. OAuth auth is added in a later migration.

CREATE TABLE mcp_servers (
    name         TEXT PRIMARY KEY,
    url          TEXT NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 0,   -- last connect/test succeeded
    auth_kind    TEXT NOT NULL,                -- 'none' | 'bearer' | 'github_app'
    bearer_token TEXT,                          -- 'bearer' only; plaintext, redacted in views
    tool_count   INTEGER,                       -- tools advertised at last successful test
    last_error   TEXT,                          -- last connect/test error, for the UI
    created_at   TEXT NOT NULL,                 -- unix epoch seconds
    updated_at   TEXT NOT NULL                  -- unix epoch seconds
);
