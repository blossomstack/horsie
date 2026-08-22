-- An index over agent runs, so "every run of preset P" is a query rather than a
-- sweep of every session's journal.
--
-- It exists because there is nowhere else to ask. A preset is *flattened* into
-- a session at creation, an agent's history lives in an opaque per-agent
-- journal, and neither can be filtered by who configured it. The journal stays
-- opaque on purpose — it is a generic event store over caller-owned bytes, and
-- "which preset ran this" is domain knowledge that has no business in it.
--
-- An index, not a mirror of the roster. It holds what a lookup needs — find the
-- session and agent, then triage by outcome and recency — and nothing else.
-- Model, label, kind, agent type and origin are all already on `sessions.get`
-- once you have the ids, and carrying them here would buy a second place for
-- them to be wrong and a write every time one changed.
--
-- Narrow is also what keeps it near-append-only: two writes per agent run,
-- ever. One when the run first appears, one when it reaches a terminal state.
-- The session actor derives rows from its own state in `on_events_persisted`
-- and writes only the difference, so a session that runs for an hour without
-- gaining or finishing an agent writes nothing at all.
--
-- `preset` is NULL for an agent configured inline. Not "" — those are different
-- answers, and an index whose commonest query is `WHERE preset = ?` must not
-- have a sentinel that could match one.
CREATE TABLE agent_runs (
    project_id TEXT    NOT NULL,
    session_id TEXT    NOT NULL,
    -- 'main', or the agent's uuid. The same vocabulary every agent-scoped
    -- route speaks, so a row is usable as an address without translation.
    agent_id   TEXT    NOT NULL,
    preset     TEXT,
    -- The roster's vocabulary: 'provisioning' | 'running' | 'idle' |
    -- 'awaiting_input' | 'completed' | 'failed' | 'cancelled'.
    status     TEXT    NOT NULL,
    -- Unix epoch ms. Zero for a main agent, which nothing spawned — its age is
    -- its session's `created_at`.
    started_at INTEGER NOT NULL,
    -- NULL while the run is still going. Distinct from the zero a
    -- `SubAgentView` carries for the same state: a nullable column can be
    -- ordered and filtered on, and `ended_at IS NULL` is the honest spelling of
    -- "still running".
    ended_at   INTEGER,
    PRIMARY KEY (project_id, session_id, agent_id)
);

-- The one query this table exists for: the recent runs of a named preset.
CREATE INDEX agent_runs_by_preset ON agent_runs (project_id, preset, started_at DESC);
