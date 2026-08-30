-- The user's inbox: what agents have addressed to the person steering them.
--
-- It exists because there is nowhere else to ask. A parked question lives as a
-- dangling `tool_use` inside one agent's opaque journal, and "which of my
-- sessions is waiting on me" is a question no journal can answer. Deriving it
-- instead would mean *loading* every session to read its status — and a loaded
-- session is a resident session, so rendering a badge would defeat idle offload
-- across the whole account.
--
-- A read model for the ask half: every row here can be rebuilt from the session
-- actor's own state, and `reconcile` at session load is what does the rebuilding.
-- The notice half has no other home — a notice is a fact the moment an agent
-- speaks it, and this table is where it is kept.
--
-- Project-wide and never joined. There is no session *name* here on purpose: a
-- client that lists the inbox already lists the sessions, so a name resolves
-- from the id it already holds — and a snapshot column would be a second copy
-- that goes stale the first time a session is renamed.
CREATE TABLE inbox_messages (
    project_id   TEXT    NOT NULL,
    id           TEXT    NOT NULL,
    -- 'notice' | 'ask'. The discriminant of the wire union, stored rather than
    -- inferred from which columns are null: a third kind must be able to arrive
    -- without every reader having to re-guess.
    kind         TEXT    NOT NULL,
    -- 'open' | 'answered' | 'declined' | 'closed'. Only 'open' can still be
    -- holding an agent.
    state        TEXT    NOT NULL,
    session_id   TEXT    NOT NULL,
    -- 'main', or the agent's uuid — the vocabulary every agent-scoped route
    -- speaks, so a row is an address without translation.
    agent_id     TEXT    NOT NULL,
    title        TEXT    NOT NULL,
    -- A notice's markdown, or an ask's question.
    body         TEXT    NOT NULL,
    -- Kind-specific remainder, as JSON. An ask's suggested `choices` and its
    -- `multiple` flag live here; a notice writes '{}'. A column each would be a
    -- migration every time a kind gains a field, and these are rendered whole
    -- rather than queried.
    payload      TEXT    NOT NULL,
    -- The parked `tool_use` an ask answers; NULL for every other kind. A real
    -- column and not part of `payload` because it is the one kind-specific
    -- thing that is *looked up* — it is the address an answer is sent to, and
    -- it is what makes re-asserting an ask row idempotent.
    tool_call_id TEXT,
    -- Unix epoch ms.
    created_at   INTEGER NOT NULL,
    -- When it was first opened; NULL means unread, which is what the badge
    -- counts. Nullable rather than a boolean because "when" is strictly more
    -- than "whether" and costs the same.
    read_at      INTEGER,
    -- When it stopped being 'open'; NULL while it still is.
    resolved_at  INTEGER,
    PRIMARY KEY (project_id, id)
);

-- One row per dangling call, however many times the projection re-asserts it.
-- Partial, because only asks have a call id and a plain unique index would let
-- exactly one notice per agent exist.
CREATE UNIQUE INDEX inbox_messages_ask_call
    ON inbox_messages (project_id, session_id, agent_id, tool_call_id)
    WHERE tool_call_id IS NOT NULL;

-- The list, which is the query this table exists for.
CREATE INDEX inbox_messages_recent ON inbox_messages (project_id, created_at DESC);

-- Resolving one agent's open asks, and dropping a deleted session's rows.
CREATE INDEX inbox_messages_by_agent ON inbox_messages (project_id, session_id, agent_id);
