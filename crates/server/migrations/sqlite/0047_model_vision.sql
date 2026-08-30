-- Which models may be shown an artifact.
--
-- Attachments reach a turn as `ArtifactRef`s. Whether the bytes behind one are
-- ever loaded is decided in exactly one place — the artifact source a session
-- hands its agent — so a text-only model gets a source that resolves nothing
-- and every provider renders the "withheld" placeholder on its own. Until now
-- that gate was hardcoded open; these columns are what it reads.
--
-- Two flags rather than one because they are two capabilities. The OpenAI chat
-- wire takes images on almost every current model but reaches PDFs by another
-- route entirely, and Anthropic's document support is per model. A single
-- `supports_vision` would be wrong about one of them on the first model that
-- has one and not the other.
--
-- INTEGER, not BOOLEAN: `sqlx::Any` decodes these as `i64`, and every other
-- flag in this schema is stored the same way (see 0013).
--
-- DEFAULT 0 is the safe direction. An existing row says nothing about its
-- model's capabilities, and showing an image to a model that cannot take one
-- is a failed turn, while withholding it is a turn that says so.
ALTER TABLE models      ADD COLUMN supports_images    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE models      ADD COLUMN supports_documents INTEGER NOT NULL DEFAULT 0;
ALTER TABLE model_cards ADD COLUMN supports_images    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE model_cards ADD COLUMN supports_documents INTEGER NOT NULL DEFAULT 0;
