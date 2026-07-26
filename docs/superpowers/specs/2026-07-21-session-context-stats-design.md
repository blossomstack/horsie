# Session context/token stats widget

Date: 2026-07-21
Status: approved, ready for planning

## Problem

The session header (`clients/web/src/pages/SessionView.tsx:157-164`) shows a
single `Gauge` chip: `stream.usage.input + stream.usage.output`, folded
client-side from replayed `TurnCompleted` SSE events
(`clients/web/src/hooks/useSessionStream.ts:217-225`). That number is a
cumulative sum across every turn in the session — useful as a rough cost
signal, but it does not answer "how full is the context window right now",
and it carries no breakdown of what the tokens actually were (fresh input vs.
prompt-cache reads/writes vs. generated output).

We want an expandable widget off that same chip showing: how full the
model's context window is right now, and a cost/cache breakdown for both the
current turn and the session total — with inline explanations, since the
cache-token distinction is not self-evident.

## What the code actually says

Verified against the tree at `main` (1186e6a):

- **`Usage` is thin.** `models/fluorite/agent.fl:53-56` has only
  `input_tokens`/`output_tokens`. Both providers already receive more than
  that and drop it:
  - `providers/anthropic/src/lib.rs:565-568` builds `Usage` from
    `async_llm::Usage`, which already carries
    `cache_creation_input_tokens`/`cache_read_input_tokens`
    (`async-llm/src/types.rs:14-23`) — parsed but discarded.
  - `providers/openai/src/wire.rs:134-141`'s `WireUsage` only deserializes
    `prompt_tokens`/`completion_tokens`. OpenAI's actual
    `/v1/chat/completions` response includes
    `usage.prompt_tokens_details.cached_tokens` for cache hits (OpenAI has no
    cache-write charge, so there is no creation-side counterpart) — not
    parsed at all today.
- **No context-window concept exists anywhere** — `max_tokens` on the
  `models` table (`server/migrations/0001_init.sql:14-18`,
  `models/fluorite/settings.fl:19-30,122-127`) is a generation cap
  (`with_max_tokens`, `agentcore/src/provider.rs:10`), unrelated to context
  size.
- **The server never aggregates usage.** Sessions are event-sourced; the only
  existing fold helpers are `replay_session_events` (maps journal entries to
  SSE wire events) and `fold_session_state` (folds `SessionDomainEvent`s for
  `pending_question`/`last_error`) in `server/src/sessions/events.rs:74-154`.
  `RunComplete { usage, iterations }` (`server/src/sessions/events.rs:125-130`)
  is mapped to `TurnCompletedEvent` per turn and journaled, but nothing sums
  it across turns server-side — the web client does that fold itself
  (`useSessionStream.ts:217-225`).
- **Precedent for server-applied defaults exists.** `default_vendor` fills an
  omitted `vendor` at the config layer already (per project memory on the
  settings/config store) — the same shape fits a context-window default.

## Scope

In scope:
- Extending `Usage` with optional cache-creation/cache-read fields, populated
  by both providers where the wire reports them.
- A `context_window` field on configured models, with a built-in default
  applied server-side for known model ids so common models work with zero
  manual setup.
- A new `GET /api/sessions/:id/stats` endpoint returning current-turn and
  session-total usage plus the model's context window.
- Turning the existing header chip into a popover (matching
  `SettingsMenu`'s open/close pattern) showing the breakdown with inline
  explanations for each field.

Out of scope, deliberately:
- **Context compaction/trimming.** This widget surfaces that the window is
  filling up; it does not do anything about it. Explicitly called out as its
  own project in `docs/superpowers/specs/2026-07-20-provider-independence-design.md:47-52`.
- **Historical/per-turn trend charts.** Only "current turn" and "session
  total" are shown — no time series, no persistence beyond the existing
  journal.
- **Cost-in-dollars.** Token counts and cache ratios only; pricing varies by
  provider/model and isn't tracked anywhere today.
- **A general model-catalog API.** `context_window` rides the existing
  `/api/config` model list; no new `/api/models` endpoint.

## Design

### 1. `Usage` gains optional cache fields

`models/fluorite/agent.fl`:

```
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
    /// Tokens written to a new prompt cache entry this turn (Anthropic only;
    /// billed at a premium — absent when the provider reports nothing).
    cache_creation_tokens: Option<u32>,
    /// Tokens served from an existing prompt cache entry this turn
    /// (Anthropic + OpenAI-compatible `cached_tokens`; billed at a discount).
    cache_read_tokens: Option<u32>,
}
```

`Option<u32>`, not `0`-defaulted, so the UI can tell "provider reported zero
cache activity" apart from "this provider doesn't report cache data at all"
(relevant once a third, non-caching backend exists).

- `providers/anthropic/src/lib.rs:565-568`: map
  `cache_creation_input_tokens`/`cache_read_input_tokens` straight through.
- `providers/openai/src/wire.rs`: add `prompt_tokens_details: Option<{
  cached_tokens: Option<u32> }>` to `WireUsage`, map to `cache_read_tokens`;
  `cache_creation_tokens` stays `None` always for this provider (no such
  concept in the wire).
- Regenerate `clients/ts` and `clients/web/src/generated` (existing
  `generate-types` scripts).

### 2. `context_window` on configured models

Mirrors `max_tokens` exactly:

- `server/migrations/0007_model_context_window.sql`:
  `ALTER TABLE models ADD COLUMN context_window INTEGER;`
- `models/fluorite/settings.fl`: add `context_window: Option<u32>` to both
  `ModelView` and `ModelInput`.
- `server/src/config/store.rs`: thread the new column through the same
  insert/select/update paths as `max_tokens` (`:396-401`, `:543-547`,
  `:612-624`, `:963-972`).
- A small built-in table, `server/src/config/model_defaults.rs`:
  `fn default_context_window(model_id: &str) -> Option<u32>`, matched by
  substring (`claude-*` → 200_000, `gpt-4o*`/`gpt-4.1*` → 128_000, etc. —
  short list, extend as needed). Applied where `ModelInput` is persisted:
  `context_window.or_else(|| default_context_window(&model_id))`. The DB
  value is always what's read back — this only fills the gap at write time,
  same shape as the existing `default_vendor` fallback. A model added via the
  Settings UI with the field left blank still gets a sane default with zero
  manual lookup; the field stays editable for anything the table doesn't
  know.
- `SettingsPage.tsx`: add a `contextWindow` field next to `maxTokens`
  (`:61`, `:94`, `:192`, `:226`, `:387`, `:839-840` — same string-draft /
  parse-on-submit pattern).

### 3. `GET /api/sessions/:id/stats`

`models/fluorite/session.fl`:

```
struct SessionStats {
    model: String,
    context_window: Option<u32>,
    /// Usage from the most recent completed turn — what's actually loaded
    /// in the model's context right now.
    current: Usage,
    /// Usage summed across every turn in the session.
    total: Usage,
    turn_count: u32,
}
```

`models/fluorite/session_api.fl`: `struct GetSessionStatsResponse { stats:
SessionStats }`.

Route: `.route("/api/sessions/:id/stats", get(handlers::get_session_stats))`
next to the existing `/api/sessions/:id` routes
(`server/src/http/mod.rs:87-90`).

Implementation, `server/src/sessions/events.rs`, a new `fold_session_usage`
alongside `fold_session_state`/`replay_session_events`: replay the agent
journal (`AgentActor::persistence_id_for`, same as `replay_session_events`),
fold every `AgentDomainEvent::RunComplete { usage, .. }` into:
- `total`: `input_tokens`/`output_tokens` summed directly; each cache field
  summed only across turns that reported `Some` for it — a field stays
  `None` in the total only if *no* turn ever reported it, so a provider
  switch mid-session (or a provider that simply never reports cache data)
  doesn't hide real numbers from turns that did report them.
- `current`: the `Usage` from the last `RunComplete` seen (fresh session with
  no completed turns yet → all-zero `Usage`, `None` cache fields).
- `turn_count`: number of `RunComplete` events folded.

`get_session_stats` handler: look up the session (existing `Get` supervisor
command, for `model`/`context_window` — the latter via a config-store lookup
by model alias) and call `fold_session_usage`, mirroring
`get_session`'s existing shape (`server/src/http/handlers.rs:154-181`).

### 4. Web UI: popover on the existing chip

- `useSessionStats(id)` in `clients/web/src/hooks/useSessions.ts`, following
  the `useSession` pattern (`:31-36`): `useQuery({ queryKey: ["session-stats",
  id], queryFn: () => api.sessions.stats(id), enabled: !!id, select: (r) =>
  r.stats })`.
- Kept fresh via invalidation, not polling: in
  `useSessionStream.ts`'s `TurnCompleted` case (`:217-225`), after the
  existing local `usage` fold, invalidate `["session-stats", sessionId]`.
  This needs the query client threaded into the hook (it currently doesn't
  use one) — pass it in via `useQueryClient()` at the `useSessionStream` call
  site or accept it as a parameter, whichever keeps the hook's existing
  signature simplest to extend.
- The header's `Gauge` chip (`SessionView.tsx:157-164`) becomes a popover
  trigger: same ref/open-state/click-outside pattern as `SettingsMenu`
  (`clients/web/src/components/SettingsMenu.tsx:6-18`), rendered as a new
  `ContextStatsPanel` component. Chip itself keeps showing the compact total
  (`compactNumber(totalTokens)`) so today's at-a-glance summary doesn't
  regress; the popover is the expansion.
- Panel contents, each stat with a short inline explanation (tooltip or
  `<p>` caption under a label — component's choice, per existing card
  density):
  - **Context window**: a filled bar, `current.input_tokens /
    context_window` (hidden — no bar, just the raw number — when
    `context_window` is `null`). Caption: "tokens currently loaded in the
    model's context."
  - **This turn**: input / output / cache-read / cache-creation, each only
    rendered when the field is non-`None` (or non-zero for the always-present
    input/output). Captions per the explanation already agreed in
    conversation: input = full prompt sent this turn; output = generated
    response; cache-read = served from cache at a discount; cache-creation =
    written to cache this turn at a premium, pays off on later turns.
  - **Session total**: same four fields, summed, plus `turn_count`.
- No change to the always-visible chip's existing "in · out" tooltip title;
  the popover supersedes it with the fuller breakdown (tooltip can stay as a
  quick hover hint even with the popover present).

## Delivery

Single PR — the pieces are small and only meaningful together (a stats
endpoint with no cache fields, or cache fields with no endpoint to surface
them, are both half-features):

1. `Usage` schema + provider mapping (Anthropic cache fields, OpenAI
   `cached_tokens`) + regenerated types.
2. `context_window` column + fluorite fields + store.rs plumbing + built-in
   defaults table + Settings UI field.
3. `fold_session_usage` + `GET /api/sessions/:id/stats` + handler tests.
4. Web UI: `useSessionStats`, invalidation wiring, `ContextStatsPanel`
   popover.

## Acceptance

- [ ] `Usage` carries optional cache-creation/cache-read fields; both
      providers populate what their wire actually reports, and leave the
      rest `None`.
- [ ] A model can have a `context_window`; known model ids get a sane
      default with no manual configuration, and it's editable in Settings.
- [ ] `GET /api/sessions/:id/stats` returns current-turn and session-total
      usage plus the model's context window, computed by folding the
      session's own journal (no new persistence).
- [ ] The session header's token chip expands into a popover showing the
      context-window fullness, current-turn breakdown, and session totals,
      each with an inline explanation of what it means.
- [ ] The popover stays live across turns without polling (SSE-driven
      invalidation).
- [ ] `make check` green; `make web` type-checks against regenerated types.
