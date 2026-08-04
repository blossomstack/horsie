-- PostgreSQL mirror of migrations/sqlite/0019_workflows.sql.
--
-- Workflows: a named graph of steps, each step an agent preset plus a fixed
-- prompt, wired by conditions over the step's structured output.
--
-- The graph is one JSON column rather than a steps table with a transitions
-- table. A definition is only ever read and written whole — every API on it is
-- a full GET or a full PUT, and a run snapshots it in one piece — so rows per
-- step would buy joins nobody performs and lose the ordering that decides
-- which transition wins.
--
-- `steps[].agent` is deliberately not a SQL foreign key: like routines, the
-- reference is validated by the service at save, which is where a useful error
-- message naming the offending step can be produced.
--
-- Numbered 0019, skipping 0018: that number is taken by the in-flight branch
-- that drops `agents.vendor`, and two migrations sharing a number is how main
-- went red once already.

CREATE TABLE workflows (
    name        TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    start       TEXT NOT NULL,          -- name of the step every run begins at
    steps       TEXT NOT NULL,          -- JSON array of WorkflowStepDef
    created_at  TEXT NOT NULL,          -- unix epoch seconds
    updated_at  TEXT NOT NULL           -- unix epoch seconds
);
