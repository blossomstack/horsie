-- An agent preset is agent configuration. Where it runs and what it runs
-- against belong to the invocation, which now supplies both as one
-- `EnvironmentSpec`. The column is neither indexed nor defaulted, so dropping
-- it is safe on both dialects.
ALTER TABLE agents DROP COLUMN repos;
