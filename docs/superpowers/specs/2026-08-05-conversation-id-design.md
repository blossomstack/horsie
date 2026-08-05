# A conversation id every provider is handed, instead of one inferred from history

Status: design, approved 2026-08-05. Branch `feat/conversation-id`.

## Problem

#209 gave the Responses provider a `prompt_cache_key` so a conversation's turns
land on the same cache — without one, the history prefix every turn re-sends is
re-read at full price, and on a ChatGPT plan that price is the subscription's
own window rather than a bill.

It derived that key from `messages[0].id`: the provider is handed no identity, so
it inferred one from the payload. That inference is wrong in two ways that will
bite.

- **A fork copies the history verbatim.** `Journal::copy_snapshot` seeds a new
  session from another's snapshot, and `hooks.fl` already lists `fork` as a
  SessionStart source. Two forked sessions therefore share `messages[0].id` and
  so share a cache key. This is not a correctness fault — `prompt_cache_key` only
  routes, while reuse stays exact-prefix matched inside one account — but the
  scope becomes an accident rather than a decision.
- **Context compaction would break it silently.** Trimming the oldest messages to
  fit a window changes `messages[0]`, so the key changes mid-conversation and the
  cache thrashes precisely on the long conversations where it matters most.
  Nothing fails; the bill just goes up.

The deeper problem is the shape: a provider inferring conversation identity from
message contents when the caller knows it outright.

## What the code actually says

- `CompletionRequest` is constructed in exactly **one** production place,
  `agentcore/src/agent.rs:322`, inside `Agent::run`.
- `Agent` is built by `AgentActor` at `workflow/src/agent_actor.rs:1945` — also
  the only production `Agent::builder` call.
- `AgentRuntimeContext.session_id` is **already the agent's identity, not the
  session's**: `session_actor.rs` passes `self.id` for a main agent and the
  subagent's own uuid (`SessionAgentKind::Sub(id)`) for a subagent. Each agent
  has its own history and therefore its own prefix, so this is exactly the
  granularity a cache key wants. The value exists and is correctly scoped; it is
  simply never passed down.
- `prompt_messages()` (`agent_actor.rs:481`) sends the whole history, filtering
  only `Hook` entries. Nothing is trimmed from the front today, which is why the
  inferred key works at all.
- `Agent::builder` has 54 call sites: 33 in `agentcore/src/agent.rs` tests, 12 in
  `tests/tests/agent_e2e.rs`, 8 in `tests/tests/provider_conformance.rs`, and the
  1 production site. There is no shared test helper to absorb them.

## Design

### 1. A required field, not an optional one

`CompletionRequest` gains `conversation_id: &'a str` — required, not
`Option`. `Agent::builder(provider, toolbox, conversation_id)` takes it as a
constructor argument rather than an optional setter.

Required, because the failure mode of any defaulting scheme is silent. The
`AgentActor` rebuilds its `Agent` on every run, so a per-`Agent` default (a fresh
uuid, say) would hand out a **new key every turn** — worse than the inferred key
it replaces — with nothing erroring. A constructor argument makes that
unrepresentable.

The production caller passes `ctx.session_id`.

### 2. Providers decide what to do with it

The Responses provider sends it as `prompt_cache_key`. The Anthropic and
chat-completions providers ignore it, exactly as they ignore other fields their
wire has no slot for. The field is named for what it *is* — the identity of this
conversation — not for the one use a single provider currently puts it to.

### 3. What is deleted

`conversation_cache_key()` and its test go with the fragility they carried. The
wire struct's `prompt_cache_key` stays `Option<String>` because the *wire* allows
omitting it, but the Responses provider now always populates it.

### 4. Fork behaviour falls out

A forked session gets its own id and therefore its own cache key: no inheritance,
no collision. Should forks later want to share the parent's warm prefix, that
becomes a deliberate argument at the fork site rather than a side effect of
copied history.

## Testing

- A provider test asserting the supplied id reaches `prompt_cache_key` verbatim.
- The "every turn of a conversation shares one cache key" test is replaced by one
  asserting the key is whatever the caller said **regardless of history** — the
  same property, now guaranteed rather than inferred.
- The conformance suite and the 53 test builders pass a literal id; this is
  mechanical.
- `make check` and `make ts-types`.

## Acceptance

- `CompletionRequest` cannot be constructed without a conversation id.
- A main agent's requests carry the session id; a subagent's carry its own uuid.
- Growing, or truncating, the history does not change the key.
- No `Option` and no fallback path remains for the id itself.
