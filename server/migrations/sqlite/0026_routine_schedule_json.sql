-- Routines: the trigger moves from three typed columns to one JSON column
-- holding the serialized `RoutineSchedule` wire union (adjacently tagged,
-- camelCase payloads). The backfill is exact string literals because we own
-- the wire shape, and the old columns always carried their payload for the
-- kind that needed it (the service enforced that at save). DROP COLUMN is
-- fine here: none of the dropped columns is indexed or has a default.
ALTER TABLE routines ADD COLUMN schedule TEXT;

UPDATE routines SET schedule = CASE schedule_kind
    WHEN 'manual' THEN '{"type":"Manual","value":{}}'
    WHEN 'every'  THEN '{"type":"Every","value":{"intervalSecs":' || interval_secs || '}}'
    WHEN 'once'   THEN '{"type":"Once","value":{"atMs":' || at_ms || '}}'
END;

ALTER TABLE routines DROP COLUMN schedule_kind;
ALTER TABLE routines DROP COLUMN interval_secs;
ALTER TABLE routines DROP COLUMN at_ms;
