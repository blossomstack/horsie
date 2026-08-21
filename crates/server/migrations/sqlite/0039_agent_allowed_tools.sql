-- The tools sessions from this preset may call, as a JSON array of tool names.
--
-- NULL is not the empty array. NULL means "the default set" — every built-in
-- group except the control plane, as this horsie version defines it — so a
-- preset saved today follows a later version's idea of sensible instead of
-- freezing this one's list. `[]` means no built-in tools at all, which is a
-- thing someone may legitimately want.
--
-- Replaces `control_plane`. That flag was a second gate answering a question the
-- selection already answers: naming a `horsie_*` tool *is* the grant, so there
-- is no longer a bit that can disagree with the list beside it. Existing grants
-- are not carried over — the tool names are generated in Rust from the control
-- plane's resource list and cannot be enumerated here, and the safe direction to
-- fail is losing authority rather than inventing it. A preset that had the
-- control plane re-grants it by selecting the Horsie tools.
ALTER TABLE agents ADD COLUMN allowed_tools TEXT;
ALTER TABLE agents DROP COLUMN control_plane;
