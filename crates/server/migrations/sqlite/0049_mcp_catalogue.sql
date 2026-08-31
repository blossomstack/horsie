-- What a server is, and what it offers, remembered between connects.
--
-- Until now a configured server was a name and a URL, and `tool_count` was the
-- only trace of the `tools/list` that had already been made. So nothing could
-- say what a server was *for*, and no picker could offer its tools — the
-- new-session screen has no MCP connection at all, and a picker that needed one
-- would be empty exactly when it matters.
--
-- `description` is what a person typed. `discovered_title` and
-- `discovered_instructions` are what the server said about itself in the
-- `initialize` handshake, which horsie previously received and threw away. The
-- typed one wins for display; the discovered ones mean the field is rarely
-- blank on a server nobody has got round to describing.
--
-- `tools` is the JSON `[{"name":…,"description":…}]` from the last successful
-- `tools/list`. NULL is not `[]`: NULL means this server has never connected,
-- `[]` means it connected and offers nothing. A failed connect leaves the last
-- known catalogue in place — a server that is down should still be describable.
--
-- `tool_count` goes, because it is now the length of `tools`. Two columns for
-- one fact is one column that can be wrong.

ALTER TABLE mcp_servers ADD COLUMN description             TEXT;
ALTER TABLE mcp_servers ADD COLUMN discovered_title        TEXT;
ALTER TABLE mcp_servers ADD COLUMN discovered_instructions TEXT;
ALTER TABLE mcp_servers ADD COLUMN tools                   TEXT;
ALTER TABLE mcp_servers DROP COLUMN tool_count;
