-- PostgreSQL mirror of migrations/sqlite/0014_auth.sql.
--
-- Authentication: one admin account plus the opaque tokens every
-- authenticated surface presents (browser cookie, CLI access/refresh, vendor
-- agent).
--
-- No REFERENCES clauses, matching the SQLite schema: enforcing a constraint on
-- one backend and not the other would make the two behave differently. See
-- 0009_memory.sql.
--
-- Timestamps here are BIGINT epoch seconds, not the TEXT epoch seconds used by
-- memory/plugins: expiry and cleanup compare them in SQL, where lexicographic
-- comparison of a TEXT number is a trap waiting for a digit change. SQLite's
-- INTEGER is 64-bit, so BIGINT is the faithful translation — PostgreSQL's
-- INTEGER would overflow in 2038.
--
-- `token_hash` and `device_code_hash` are BYTEA where SQLite has BLOB. Both map
-- to `AnyValueKind::Blob`, so the Rust side is unchanged.
--
-- `password_is_generated` stays INTEGER rather than BOOLEAN; see 0003_mcp.sql.

CREATE TABLE auth_users (
    id                    BIGSERIAL PRIMARY KEY,
    username              TEXT NOT NULL UNIQUE,
    password_hash         TEXT NOT NULL,           -- argon2id PHC string
    -- 1 while the first-boot generated password is still in use
    password_is_generated INTEGER NOT NULL DEFAULT 0,
    created_at            BIGINT NOT NULL,
    updated_at            BIGINT NOT NULL
);

CREATE TABLE auth_tokens (
    id           TEXT PRIMARY KEY,       -- public uuid; safe to list and log
    kind         TEXT NOT NULL,          -- web | access | refresh | agent
    principal    TEXT NOT NULL,          -- user:<id> | agent:<token id>
    token_hash   BYTEA NOT NULL UNIQUE,  -- SHA-256 of the presented secret
    label        TEXT,                   -- agent tokens: operator-chosen name
    chain_id     TEXT,                   -- access/refresh: rotation chain
    expires_at   BIGINT,                 -- NULL = never (agent tokens)
    created_at   BIGINT NOT NULL,
    last_used_at BIGINT,
    revoked_at   BIGINT
);

CREATE INDEX idx_auth_tokens_hash ON auth_tokens(token_hash);
CREATE INDEX idx_auth_tokens_chain ON auth_tokens(chain_id);
CREATE INDEX idx_auth_tokens_principal ON auth_tokens(principal, kind);

CREATE TABLE auth_device_codes (
    device_code_hash BYTEA PRIMARY KEY,
    user_code        TEXT NOT NULL UNIQUE,
    principal        TEXT,              -- set on approval
    created_at       BIGINT NOT NULL,
    expires_at       BIGINT NOT NULL,
    approved_at      BIGINT,
    denied_at        BIGINT,
    consumed_at      BIGINT,
    last_polled_at   BIGINT             -- drives slow_down
);
