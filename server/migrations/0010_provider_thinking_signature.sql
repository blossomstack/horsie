-- Whether to retain thinking-block signatures captured from this provider.
-- Genuine Anthropic validates signatures when thinking blocks are replayed;
-- Anthropic-compatible endpoints (Kimi, z.ai) do not. The signatures are opaque
-- 4-13 KB blobs that no client reads, so the default is off and real Anthropic
-- deployments opt back in.
ALTER TABLE providers ADD COLUMN keep_thinking_signature INTEGER NOT NULL DEFAULT 0;
