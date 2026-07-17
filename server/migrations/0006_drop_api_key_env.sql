-- Inline secrets only now — the env-var indirection was a second,
-- silently-broken-until-restart failure mode with no remaining benefit once
-- the DB path is the supported one (see the live-vendor-activation spec).
ALTER TABLE providers DROP COLUMN api_key_env;
