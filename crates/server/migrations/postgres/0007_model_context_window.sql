-- PostgreSQL mirror of migrations/sqlite/0007_model_context_window.sql.
--
-- The model's context window size, distinct from `max_tokens` (a generation
-- cap). Nullable: a built-in default is applied at write time for known
-- model ids (see `default_context_window` in store.rs), but stays editable.
ALTER TABLE models ADD COLUMN context_window INTEGER;
