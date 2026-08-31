-- PostgreSQL mirror of migrations/sqlite/0049_routine_target.sql. See there for
-- why the target is one JSON column rather than two nullable ones.
ALTER TABLE routines ADD COLUMN target TEXT;

UPDATE routines SET target = '{"type":"Agent","value":{"agent":"' || agent || '"}}';

ALTER TABLE routines DROP COLUMN agent;
