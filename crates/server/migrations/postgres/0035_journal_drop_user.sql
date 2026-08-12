-- A journal log is found by `(kind, id)` alone.
--
-- `user_id` was added by 0024 on the reasoning that "two accounts may each run
-- an actor with the same persistence id" -- true in principle, and false here:
-- an account id is random, a session and an agent are uuids. Nothing has ever
-- collided and nothing can.
--
-- What forces it out is that a `Journal` method receives a `PersistenceId` and
-- nothing else, and a persistence id is fixed when its actor is *constructed* --
-- which for a clustered actor is before a single byte of its history has been
-- read. So the account could only reach the journal by being packed into the id,
-- and `Journal` is a framework trait whose users do not all have accounts.
--
-- What the column bought was that another account's log stayed unreachable even
-- given its id. That protection is now that the id is a uuid nothing hands out.
-- Deleting an account's data walks its supervisor's session list.
--
-- The unique key narrows back to what 0017 created it as.
ALTER TABLE journal_logs DROP CONSTRAINT journal_logs_user_id_kind_id_key;
ALTER TABLE journal_logs ADD UNIQUE (kind, id);
ALTER TABLE journal_logs DROP COLUMN user_id;
