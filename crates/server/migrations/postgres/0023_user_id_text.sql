-- PostgreSQL mirror of migrations/sqlite/0023_user_id_text.sql.
--
-- A user id is a short random string, not an autoincrementing integer: a
-- sequential key published as a scope leaks how many accounts a deployment has
-- and makes the set enumerable. The single bootstrap row keeps `'1'`, the text
-- of the integer it had.
--
-- PostgreSQL can retype in place, so no rebuild is needed. The column was
-- BIGSERIAL, which is a BIGINT plus a DEFAULT drawn from an owned sequence:
-- dropping the default first is what lets the type change, and the sequence
-- goes with it since nothing will draw from it again.

ALTER TABLE auth_users ALTER COLUMN id DROP DEFAULT;
ALTER TABLE auth_users ALTER COLUMN id TYPE TEXT USING id::TEXT;
DROP SEQUENCE IF EXISTS auth_users_id_seq;
