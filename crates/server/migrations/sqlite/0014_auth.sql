-- Authentication: one admin account plus the opaque tokens every
-- authenticated surface presents (browser cookie, CLI access/refresh, vendor
-- agent). Sub-project A mints only `web` tokens; the table is shared so the
-- CLI and vendor work does not reshape it.
--
-- No REFERENCES clauses: `PRAGMA foreign_keys` is never enabled in
-- `open_pool`, so a declared constraint would be silently ignored -- worse
-- than no constraint at all. See 0009_memory.sql.
--
-- Timestamps here are INTEGER epoch seconds, not the TEXT epoch seconds used
-- by memory/plugins: expiry and cleanup compare them in SQL, where
-- lexicographic comparison of a TEXT number is a trap waiting for a digit
-- change.

CREATE TABLE auth_users (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    username              TEXT NOT NULL UNIQUE,
    password_hash         TEXT NOT NULL,           -- argon2id PHC string
    -- 1 while the first-boot generated password is still in use
    password_is_generated INTEGER NOT NULL DEFAULT 0,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL
);

CREATE TABLE auth_tokens (
    id           TEXT PRIMARY KEY,      -- public uuid; safe to list and log
    kind         TEXT NOT NULL,         -- web | access | refresh | agent
    principal    TEXT NOT NULL,         -- user:<id> | agent:<token id>
    token_hash   BLOB NOT NULL UNIQUE,  -- SHA-256 of the presented secret
    label        TEXT,                  -- agent tokens: operator-chosen name
    chain_id     TEXT,                  -- access/refresh: rotation chain
    expires_at   INTEGER,               -- NULL = never (agent tokens)
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER,
    revoked_at   INTEGER
);

CREATE INDEX idx_auth_tokens_hash ON auth_tokens(token_hash);
CREATE INDEX idx_auth_tokens_chain ON auth_tokens(chain_id);
CREATE INDEX idx_auth_tokens_principal ON auth_tokens(principal, kind);

-- Unused until the CLI device flow (sub-project B) lands. Created here so the
-- auth schema arrives as one migration rather than reshaping a shipped table.
CREATE TABLE auth_device_codes (
    device_code_hash BLOB PRIMARY KEY,
    user_code        TEXT NOT NULL UNIQUE,
    principal        TEXT,              -- set on approval
    created_at       INTEGER NOT NULL,
    expires_at       INTEGER NOT NULL,
    approved_at      INTEGER,
    denied_at        INTEGER,
    consumed_at      INTEGER,
    last_polled_at   INTEGER            -- drives slow_down
);
