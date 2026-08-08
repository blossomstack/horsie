-- PostgreSQL mirror of migrations/sqlite/0018_agent_vendor.sql.
--
-- Drop `agents.vendor`. A preset no longer names a runtime vendor: which
-- machine runs the work belongs to the invocation, not to the saved
-- configuration. A pinned vendor is invisible once it disconnects but fatal at
-- invoke, which surfaces as a routine that silently stopped working.
--
-- Every invocation now resolves the server's default vendor instead.

ALTER TABLE agents DROP COLUMN vendor;
