-- A routine's target moves from an agent name to one JSON column holding the
-- serialized `RoutineTarget` wire union — the same shape 0026 gave the
-- schedule, and for the same reason: a routine now runs either an agent preset
-- or a workflow, and two nullable columns could express "neither" and "both".
--
-- The backfill is an exact string literal because we own the wire shape, and
-- `agent` was NOT NULL and always carried a preset name. Adjacently tagged,
-- camelCase payloads, as everywhere else on the wire.
ALTER TABLE routines ADD COLUMN target TEXT;

UPDATE routines SET target = '{"type":"Agent","value":{"agent":"' || agent || '"}}';

ALTER TABLE routines DROP COLUMN agent;
