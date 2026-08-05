-- A user id is a short random string, not an autoincrementing integer: a
-- sequential key published as a scope leaks how many accounts a deployment has
-- and makes the set enumerable.
--
-- SQLite cannot retype a column or alter a primary key, so this rebuilds the
-- table. The single bootstrap row keeps `'1'` -- the text of the integer it had,
-- a legitimate id rather than a sentinel. Accounts created after this migration
-- get a random one from `create_user`.
--
-- No REFERENCES clauses: `PRAGMA foreign_keys` is never enabled in `open_pool`,
-- so a declared constraint would be silently ignored. See 0009_memory.sql.

CREATE TABLE auth_users_new (
    id                    TEXT PRIMARY KEY,
    username              TEXT NOT NULL UNIQUE,
    password_hash         TEXT NOT NULL,
    password_is_generated INTEGER NOT NULL DEFAULT 0,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL
);

INSERT INTO auth_users_new (id, username, password_hash, password_is_generated,
                            created_at, updated_at)
SELECT CAST(id AS TEXT), username, password_hash, password_is_generated,
       created_at, updated_at
FROM auth_users;

DROP TABLE auth_users;
ALTER TABLE auth_users_new RENAME TO auth_users;
