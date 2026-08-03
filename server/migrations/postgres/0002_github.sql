-- PostgreSQL mirror of migrations/sqlite/0002_github.sql.
--
-- Deployment-global GitHub connection. Single-row tables (no user/tenant
-- layer): the App the deployment authenticates as, and the one connected
-- account's OAuth credentials. Secrets are stored plaintext like provider
-- api_keys (the database is the trust boundary); views redact them.

CREATE TABLE github_app (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    client_id     TEXT NOT NULL,
    client_secret TEXT,
    app_id        INTEGER,
    private_key   TEXT,            -- PEM, raw or base64-encoded
    app_slug      TEXT,
    callback_base TEXT             -- e.g. "https://horsie.example.com"; NULL → derive from request
);

CREATE TABLE github_credentials (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    login           TEXT NOT NULL,
    access_token    TEXT NOT NULL,
    refresh_token   TEXT,
    expires_at      TEXT,           -- RFC 3339
    installation_id INTEGER
);
