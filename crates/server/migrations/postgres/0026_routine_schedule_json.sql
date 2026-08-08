-- PostgreSQL mirror of migrations/sqlite/0026_routine_schedule_json.sql.
-- Same shape; PostgreSQL combines the three drops into one statement.
ALTER TABLE routines ADD COLUMN schedule TEXT;

UPDATE routines SET schedule = CASE schedule_kind
    WHEN 'manual' THEN '{"type":"Manual","value":{}}'
    WHEN 'every'  THEN '{"type":"Every","value":{"intervalSecs":' || interval_secs || '}}'
    WHEN 'once'   THEN '{"type":"Once","value":{"atMs":' || at_ms || '}}'
END;

ALTER TABLE routines DROP COLUMN schedule_kind,
                     DROP COLUMN interval_secs,
                     DROP COLUMN at_ms;
