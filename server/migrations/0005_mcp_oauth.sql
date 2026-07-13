-- OAuth 2.1 auth for MCP servers (`auth_kind = 'oauth'`). Tokens and the client
-- secret are stored plaintext like provider api_keys / github credentials (the
-- DB file is the trust boundary) and wrapped in Secret + redacted in views.
-- `oauth_meta` caches the discovered authorization-server endpoints (JSON) so
-- the callback and refresh never re-run discovery. `oauth_expires_at` is a
-- unix-epoch-seconds string, matching the github credentials convention.

ALTER TABLE mcp_servers ADD COLUMN oauth_client_id     TEXT;
ALTER TABLE mcp_servers ADD COLUMN oauth_client_secret TEXT;
ALTER TABLE mcp_servers ADD COLUMN oauth_access_token  TEXT;
ALTER TABLE mcp_servers ADD COLUMN oauth_refresh_token TEXT;
ALTER TABLE mcp_servers ADD COLUMN oauth_expires_at    TEXT;
ALTER TABLE mcp_servers ADD COLUMN oauth_meta          TEXT;
