-- OAuth credentials for providers that sign in rather than carry an API key —
-- today only `kind = 'chatgpt'`, which spends a ChatGPT subscription.
--
-- Separate from `providers.api_key` because these rotate: every refresh writes
-- a new access token and may rotate the refresh token, whereas `api_key` is a
-- write-only field an operator sets by hand and never changes on its own.
--
-- Timestamps are INTEGER epoch seconds, as in 0014_auth.sql: expiry is compared
-- in SQL, and lexicographic comparison of a TEXT number is a trap waiting for a
-- digit change.
--
-- No REFERENCES on `provider`: `PRAGMA foreign_keys` is never enabled in
-- `open_pool`, so a declared constraint would be silently ignored — worse than
-- no constraint at all. Deleting a provider deletes its credential explicitly.
CREATE TABLE provider_oauth (
    provider   TEXT PRIMARY KEY,   -- providers.name
    access     TEXT NOT NULL,
    refresh    TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    account_id TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
