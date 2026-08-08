-- PostgreSQL mirror of migrations/sqlite/0028_routine_environment.sql.
ALTER TABLE routines ADD COLUMN environment TEXT NOT NULL
    DEFAULT '{"type":"Runtime","value":{"vendor":"local"}}';
