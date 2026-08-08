-- OAuth credentials for providers that sign in rather than carry an API key —
-- today only `kind = 'chatgpt'`, which spends a ChatGPT subscription.
--
-- Separate from `providers.api_key` because these rotate: every refresh writes
-- a new access token and may rotate the refresh token, whereas `api_key` is a
-- write-only field an operator sets by hand and never changes on its own.
--
-- Timestamps are epoch seconds, matching the SQLite dialect and 0014_auth.sql.
CREATE TABLE provider_oauth (
    provider   TEXT PRIMARY KEY,   -- providers.name
    access     TEXT NOT NULL,
    refresh    TEXT NOT NULL,
    expires_at BIGINT NOT NULL,
    account_id TEXT NOT NULL,
    updated_at BIGINT NOT NULL
);
