-- An agent preset could say what it was *for* but not how its agent should
-- behave: `description` is roster copy and never reaches the model, so two
-- presets on one model were behaviourally identical.
ALTER TABLE agents ADD COLUMN instructions TEXT;
