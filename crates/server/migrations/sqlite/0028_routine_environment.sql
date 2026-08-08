-- Routines gain the environment every run happens in, stored verbatim as the
-- serialized `EnvironmentSpec` wire union (adjacently tagged, camelCase
-- payloads) — the convention 0026 established for the schedule.
--
-- Existing rows ran on whatever the server's default vendor resolved to at
-- trigger time, which a migration cannot read. They take the local runtime:
-- the vendor a self-hosted install almost always defaults to, and the one whose
-- absence is immediately obvious in the routine's `last_error` if it is wrong.
ALTER TABLE routines ADD COLUMN environment TEXT NOT NULL
    DEFAULT '{"type":"Runtime","value":{"vendor":"local"}}';
