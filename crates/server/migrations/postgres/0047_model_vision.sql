-- PostgreSQL mirror of migrations/sqlite/0047_model_vision.sql. See there for
-- why the gate needs these columns, and why it is two flags and not one.
--
-- INTEGER rather than BOOLEAN; see 0003_mcp.sql.
ALTER TABLE models      ADD COLUMN supports_images    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE models      ADD COLUMN supports_documents INTEGER NOT NULL DEFAULT 0;
ALTER TABLE model_cards ADD COLUMN supports_images    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE model_cards ADD COLUMN supports_documents INTEGER NOT NULL DEFAULT 0;
