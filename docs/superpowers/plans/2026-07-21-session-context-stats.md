# Session Context/Token Stats Widget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the session header's token chip into an expandable popover showing context-window fullness and an input/output/cache-read/cache-creation breakdown, for the current turn and the session total, each with an inline explanation — backed by a new `GET /api/sessions/:id/stats` endpoint.

**Architecture:** `Usage` (the wire type shared by both LLM providers and every session event) grows two optional cache-token fields, populated by the Anthropic and OpenAI-compatible providers from data they already receive but currently drop. A `context_window` column joins the existing `max_tokens` column on the `models` table, with a small built-in default table so common models need no manual setup. A new server-side fold over the session's existing event-sourced journal (mirroring the existing `fold_session_state` helper) computes current-turn and session-total usage on demand — no new persistence. The web client fetches this via a new React Query hook, invalidated on every `TurnCompleted` SSE event, and renders it in a popover off the existing header chip.

**Tech Stack:** Rust (axum, sqlx/SQLite, fluorite codegen), React 19 + TanStack Query + Tailwind v4 (Bun/Vite).

## Global Constraints

- Design source of truth: `docs/superpowers/specs/2026-07-21-session-context-stats-design.md`.
- `Usage`'s new cache fields are `Option<u32>`, never defaulted to `0` — `None` means "this provider/turn reported no cache data," which must stay distinguishable from a real zero.
- No new persistence: session stats are computed by replaying the existing agent journal, exactly like `fold_session_state`/`replay_session_events` already do.
- `context_window` mirrors the existing `max_tokens` column/field pattern exactly (same struct, same store.rs plumbing shape, same Settings UI row).
- Every Rust step must leave `cargo build --workspace --all-features` (at minimum) green before moving on — `Usage` gaining fields is a breaking change to every existing struct literal.
- Regenerate TypeScript types (`bun run generate-types` in `clients/web`, and `npm run generate-types && npm run typecheck` in `clients/ts`) any time a `.fl` schema changes, per `Makefile:57-68`.

---

### Task 1: `Usage` schema, constructor, and cache-token summation in the agent loop

**Files:**
- Modify: `models/fluorite/agent.fl` (the `Usage` struct)
- Modify: `models/src/lib.rs` (add a hand-written `Usage::without_cache` constructor)
- Modify: `agentcore/src/agent.rs:253-263,318-319` (the per-run `total_usage` accumulator)
- Test: `agentcore/src/agent.rs` (new test in the existing `mod tests`)

**Interfaces:**
- Produces: `Usage::without_cache(input_tokens: u32, output_tokens: u32) -> Usage` (cache fields `None`) — every other task that needs a cache-less `Usage` uses this instead of a 4-field struct literal.
- Produces: `Usage { input_tokens, output_tokens, cache_creation_tokens: Option<u32>, cache_read_tokens: Option<u32> }` as the full field set — Tasks 2 and 4 construct this directly with real values.

- [ ] **Step 1: Add the two fields to the schema**

Edit `models/fluorite/agent.fl`:

```
/// Token usage for a model turn
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
    /// Tokens written to a new prompt-cache entry this turn (Anthropic only;
    /// billed at a premium). Absent when the provider reports no cache data.
    cache_creation_tokens: Option<u32>,
    /// Tokens served from an existing prompt-cache entry this turn
    /// (Anthropic + OpenAI-compatible `cached_tokens`; billed at a discount).
    /// Absent when the provider reports no cache data.
    cache_read_tokens: Option<u32>,
}
```

- [ ] **Step 2: Add a hand-written constructor**

`models/src/lib.rs` currently has:

```rust
pub mod agent {
    include!(concat!(env!("OUT_DIR"), "/agent/mod.rs"));
}
```

Change it to:

```rust
pub mod agent {
    include!(concat!(env!("OUT_DIR"), "/agent/mod.rs"));

    impl Usage {
        /// A `Usage` with no cache data reported — the common case for test
        /// fixtures and any call site that doesn't yet know about caching.
        pub fn new(input_tokens: u32, output_tokens: u32) -> Self {
            Self {
                input_tokens,
                output_tokens,
                cache_creation_tokens: None,
                cache_read_tokens: None,
            }
        }
    }
}
```

- [ ] **Step 3: Confirm the workspace fails to compile (proves the field addition is live)**

Run: `cargo build --workspace --all-features 2>&1 | head -50`
Expected: FAIL — multiple "missing fields `cache_creation_tokens`, `cache_read_tokens`" errors, starting with `agentcore/src/agent.rs:253`.

- [ ] **Step 4: Fix the per-run accumulator and sum cache tokens across iterations**

`agentcore/src/agent.rs:253-256` currently reads:

```rust
        let mut total_usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
        };
```

Replace with:

```rust
        let mut total_usage = Usage::without_cache(0, 0);
```

`agentcore/src/agent.rs:318-319` currently reads:

```rust
            total_usage.input_tokens += response.usage.input_tokens;
            total_usage.output_tokens += response.usage.output_tokens;
```

Replace with:

```rust
            total_usage.input_tokens += response.usage.input_tokens;
            total_usage.output_tokens += response.usage.output_tokens;
            total_usage.cache_creation_tokens = sum_optional(
                total_usage.cache_creation_tokens,
                response.usage.cache_creation_tokens,
            );
            total_usage.cache_read_tokens =
                sum_optional(total_usage.cache_read_tokens, response.usage.cache_read_tokens);
```

Add this free function near the top of `agentcore/src/agent.rs` (module level, above the `impl Agent` block that contains `total_usage`):

```rust
/// Sums two optional per-turn cache-token counts. Stays `None` only when
/// *neither* side reported anything — a turn/provider that's silent about
/// cache data shouldn't zero out a total another turn already contributed to.
fn sum_optional(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}
```

- [ ] **Step 5: Write the failing test for cache summation**

Add to `agentcore/src/agent.rs`'s `mod tests` (near `test_tool_call_cycle`):

```rust
    #[tokio::test]
    async fn test_run_complete_usage_sums_cache_tokens_across_iterations() {
        let provider = MockProvider::new(vec![
            CompletionResponse {
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "tc1".into(),
                    name: "search".into(),
                    input: json!({}),
                })],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 10,
                    cache_creation_tokens: Some(15),
                    cache_read_tokens: None,
                },
            },
            CompletionResponse {
                parts: vec![ContentPart::Text(TextPart {
                    text: "done".into(),
                })],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 30,
                    output_tokens: 8,
                    cache_creation_tokens: None,
                    cache_read_tokens: Some(25),
                },
            },
        ]);
        let toolbox = MockToolbox::echo("search");
        let mut agent = Agent::builder(provider, toolbox).build().unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("msg-1", "x"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let usage = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::RunComplete(rc) => Some(rc.usage),
                _ => None,
            })
            .unwrap();
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.output_tokens, 18);
        assert_eq!(usage.cache_creation_tokens, Some(15));
        assert_eq!(usage.cache_read_tokens, Some(25));
    }
```

- [ ] **Step 6: Run it**

Run: `cargo test -p horsie-agentcore test_run_complete_usage_sums_cache_tokens_across_iterations`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add models/fluorite/agent.fl models/src/lib.rs agentcore/src/agent.rs
git commit -m "agentcore: add cache-token fields to Usage, sum them across turn iterations"
```

---

### Task 2: Fix every remaining `Usage` call site (mechanical, compiler-driven)

**Files:**
- Modify: `agentcore/src/testkit.rs:41-44,57-60,67-70`
- Modify: `agentcore/src/agent.rs` (14 test-fixture literals inside `mod tests`, at lines 741, 805, 817, 864, 949, 988, 1027, 1080, 1140, 1175, 1235, 1346, 1356, 1398 as of this plan's writing — the compiler is the source of truth for the exact current set)
- Modify: `providers/openai/src/lib.rs:211-214` (`StreamState::default`)
- Modify: `workflow/src/agent_actor.rs:1276-1279` (one test fixture)

**Interfaces:**
- Consumes: `Usage::without_cache(input_tokens, output_tokens)` from Task 1.

- [ ] **Step 1: Replace every remaining two-field `Usage { input_tokens: X, output_tokens: Y }` literal with `Usage::without_cache(X, Y)`**

Every site left uncompilable by Task 1 follows this exact shape:

```rust
// before
usage: Usage {
    input_tokens: 20,
    output_tokens: 10,
},

// after
usage: Usage::without_cache(20, 10),
```

Apply this transform (same field values, just collapsed to the constructor call) at:
- `agentcore/src/testkit.rs:41-44`, `:57-60`, `:67-70`
- `agentcore/src/agent.rs`: all 14 test-module sites listed above
- `providers/openai/src/lib.rs:211-214` — this one is a named field, not a struct literal in a call: change
  ```rust
  // `Usage` has no `Default` impl in the models crate.
  usage: Usage {
      input_tokens: 0,
      output_tokens: 0,
  },
  ```
  to
  ```rust
  usage: Usage::without_cache(0, 0),
  ```
  (the comment explaining the missing `Default` impl no longer applies — delete it.)
- `workflow/src/agent_actor.rs:1276-1279`

- [ ] **Step 2: Build until clean**

Run: `cargo build --workspace --all-features 2>&1 | grep -E "^error" `
Expected: no output. If any `error[E0063]: missing field` remains, it's a site this step's list missed — apply the same transform and re-run.

- [ ] **Step 3: Run the full test suite to confirm nothing else broke**

Run: `cargo test --workspace --all-features`
Expected: PASS (same pass count as before Task 1, plus the one new test from Task 1).

- [ ] **Step 4: Commit**

```bash
git add agentcore/src/testkit.rs agentcore/src/agent.rs providers/openai/src/lib.rs workflow/src/agent_actor.rs
git commit -m "agentcore,openai,workflow: migrate Usage literals to Usage::without_cache"
```

---

### Task 3: Anthropic provider — capture cache tokens from the stream

**Files:**
- Modify: `providers/anthropic/src/lib.rs:344-347,362-363,395-399,565-568`
- Test: `providers/anthropic/tests/integration.rs`

**Interfaces:**
- Consumes: `async_llm::types::Usage.cache_creation_input_tokens: Option<u32>` and `.cache_read_input_tokens: Option<u32>` (already deserialized by the `async-llm` fork, currently unread — confirmed at `async-llm/src/types.rs:14-23`).
- Produces: `CompletionResponse.usage.cache_creation_tokens` / `.cache_read_tokens` populated whenever Anthropic's `message_start` frame reports them.

- [ ] **Step 1: Write the failing regression test**

Add to `providers/anthropic/tests/integration.rs` (near `test_text_response`):

```rust
#[tokio::test]
async fn test_text_response_has_no_cache_tokens_when_wire_omits_them() {
    let mock = MockLlmServer::builder()
        .response("Hello world")
        .build()
        .await;
    let p = provider_at(&mock.url());
    let msgs = user_messages("hi");
    let (sink, _events) = collect_sink();
    let resp = p
        .complete(no_tools_request(&msgs), "msg-1", &sink)
        .await
        .unwrap();
    // `MockLlmServer`'s fixed message_start frame never sets cache fields —
    // they must surface as `None`, not `Some(0)`.
    assert_eq!(resp.usage.cache_creation_tokens, None);
    assert_eq!(resp.usage.cache_read_tokens, None);
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p horsie-anthropic test_text_response_has_no_cache_tokens_when_wire_omits_them`
Expected: FAIL — `resp.usage` has no `cache_creation_tokens` field yet (compile error, since Task 1/2 already added the fields to the struct but this provider still constructs `Usage` with only 2 fields at line 565-568 — the build itself will fail here, which is the "red" state for this step).

- [ ] **Step 3: Capture the cache fields alongside `input_tokens`**

`providers/anthropic/src/lib.rs:344-347` currently declares:

```rust
        let mut stop_reason = StopReason::EndTurn;
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;
        let mut last_error: Option<LlmError> = None;
```

Add two more accumulators:

```rust
        let mut stop_reason = StopReason::EndTurn;
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;
        let mut cache_creation_tokens: Option<u32> = None;
        let mut cache_read_tokens: Option<u32> = None;
        let mut last_error: Option<LlmError> = None;
```

`:362-363` (the retry-branch reset) currently reads:

```rust
                input_tokens = 0;
                output_tokens = 0;
```

Add:

```rust
                input_tokens = 0;
                output_tokens = 0;
                cache_creation_tokens = None;
                cache_read_tokens = None;
```

`:395-399` currently reads:

```rust
                    MessagesStreamEvent::MessageStart { message, usage: _ } => {
                        if let Some(u) = &message.usage {
                            input_tokens = u.input_tokens.unwrap_or(0);
                        }
                        None
                    }
```

Change to:

```rust
                    MessagesStreamEvent::MessageStart { message, usage: _ } => {
                        if let Some(u) = &message.usage {
                            input_tokens = u.input_tokens.unwrap_or(0);
                            cache_creation_tokens = u.cache_creation_input_tokens;
                            cache_read_tokens = u.cache_read_input_tokens;
                        }
                        None
                    }
```

`:565-568` currently reads:

```rust
            usage: Usage {
                input_tokens,
                output_tokens,
            },
```

Change to:

```rust
            usage: Usage {
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
            },
```

- [ ] **Step 4: Run the test again**

Run: `cargo test -p horsie-anthropic test_text_response_has_no_cache_tokens_when_wire_omits_them`
Expected: PASS.

- [ ] **Step 5: Run the full provider test suite to confirm no regressions**

Run: `cargo test -p horsie-anthropic`
Expected: PASS (all existing tests plus the new one).

- [ ] **Step 6: Commit**

```bash
git add providers/anthropic/src/lib.rs providers/anthropic/tests/integration.rs
git commit -m "anthropic: surface cache_creation/cache_read tokens from message_start usage"
```

---

### Task 4: OpenAI-compatible provider — parse `cached_tokens`

**Files:**
- Modify: `providers/openai/src/wire.rs:134-141`
- Modify: `providers/openai/src/lib.rs:232-235`
- Test: `providers/openai/src/wire.rs` (new test in the existing `mod tests`)

**Interfaces:**
- Consumes: OpenAI's `/v1/chat/completions` response `usage.prompt_tokens_details.cached_tokens` — reported by real OpenAI, absent on backends that don't support caching (Ollama, vLLM, llama.cpp), hence `#[serde(default)]` throughout per this crate's existing lenient-parsing convention (`providers/openai/src/wire.rs:1-8`).
- Produces: `StreamState.usage.cache_read_tokens` populated when a chunk's `usage.prompt_tokens_details.cached_tokens` is present. `cache_creation_tokens` stays `None` always for this provider — OpenAI has no cache-write concept.

- [ ] **Step 1: Write the failing test**

Add to `providers/openai/src/wire.rs`'s `mod tests` (near the top, after the existing `use` lines):

```rust
    #[test]
    fn wire_usage_parses_cached_tokens_when_present() {
        let json = r#"{
            "prompt_tokens": 2006,
            "completion_tokens": 300,
            "prompt_tokens_details": { "cached_tokens": 1920 }
        }"#;
        let u: WireUsage = serde_json::from_str(json).unwrap();
        assert_eq!(u.prompt_tokens, 2006);
        assert_eq!(u.completion_tokens, 300);
        assert_eq!(u.cached_tokens(), Some(1920));
    }

    #[test]
    fn wire_usage_cached_tokens_absent_when_backend_omits_the_field() {
        let json = r#"{"prompt_tokens": 10, "completion_tokens": 5}"#;
        let u: WireUsage = serde_json::from_str(json).unwrap();
        assert_eq!(u.cached_tokens(), None);
    }
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p horsie-openai wire_usage_parses_cached_tokens_when_present`
Expected: FAIL — `WireUsage` has no `cached_tokens()` method or `prompt_tokens_details` field yet.

- [ ] **Step 3: Add the field and accessor**

`providers/openai/src/wire.rs:134-141` currently reads:

```rust
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}
```

Change to:

```rust
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

impl WireUsage {
    /// Tokens served from the backend's prompt cache, when it reports them.
    /// OpenAI proper always includes `prompt_tokens_details`; Ollama/vLLM/
    /// llama.cpp typically omit it entirely — both cases are `None` here,
    /// not `Some(0)`.
    pub fn cached_tokens(&self) -> Option<u32> {
        self.prompt_tokens_details.as_ref()?.cached_tokens
    }
}
```

- [ ] **Step 4: Run the tests again**

Run: `cargo test -p horsie-openai wire_usage_parses_cached_tokens_when_present wire_usage_cached_tokens_absent_when_backend_omits_the_field`
Expected: PASS.

- [ ] **Step 5: Wire it into `absorb_chunk`**

`providers/openai/src/lib.rs:232-235` currently reads:

```rust
        if let Some(u) = &chunk.usage {
            state.usage.input_tokens = u.prompt_tokens;
            state.usage.output_tokens = u.completion_tokens;
        }
```

Change to:

```rust
        if let Some(u) = &chunk.usage {
            state.usage.input_tokens = u.prompt_tokens;
            state.usage.output_tokens = u.completion_tokens;
            state.usage.cache_read_tokens = u.cached_tokens();
        }
```

- [ ] **Step 6: Run the full crate test suite**

Run: `cargo test -p horsie-openai`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add providers/openai/src/wire.rs providers/openai/src/lib.rs
git commit -m "openai: parse prompt_tokens_details.cached_tokens into cache_read_tokens"
```

---

### Task 5: `context_window` on configured models

**Files:**
- Create: `server/migrations/0007_model_context_window.sql`
- Modify: `models/fluorite/settings.fl` (`ModelView`, `ModelInput`)
- Modify: `server/src/config/store.rs` (`ModelRow`, `read_models`, `model_view`, the `INSERT` in `update`, plus a new `default_context_window` helper)
- Test: `server/src/config/store.rs` (extend the existing `mod tests`)

**Interfaces:**
- Produces: `ModelView.context_window: Option<u32>` on `GET /api/config`, `ModelInput.context_window: Option<u32>` on `PUT /api/config` — consumed by Task 6 (the Settings UI) and Task 9 (the stats handler, via `ConfigStore::view()`).

- [ ] **Step 1: Add the migration**

Create `server/migrations/0007_model_context_window.sql`:

```sql
-- The model's context window size, distinct from `max_tokens` (a generation
-- cap). Nullable: a built-in default is applied at write time for known
-- model ids (see `default_context_window` in store.rs), but stays editable.
ALTER TABLE models ADD COLUMN context_window INTEGER;
```

- [ ] **Step 2: Add the field to both fluorite structs**

`models/fluorite/settings.fl:19-30` currently reads:

```
struct ModelView {
    /// The alias sessions select (e.g. "sonnet").
    alias: String,
    /// Name of the provider this model routes to.
    provider: String,
    /// The provider's model identifier (e.g. "claude-sonnet-4-6").
    model_id: String,
    max_tokens: Option<u32>,
}
```

Add a field:

```
struct ModelView {
    /// The alias sessions select (e.g. "sonnet").
    alias: String,
    /// Name of the provider this model routes to.
    provider: String,
    /// The provider's model identifier (e.g. "claude-sonnet-4-6").
    model_id: String,
    max_tokens: Option<u32>,
    /// The model's total context window, in tokens. A built-in default is
    /// applied for known model ids when a model is added with this omitted.
    context_window: Option<u32>,
}
```

`models/fluorite/settings.fl:122-127` currently reads:

```
/// A model alias to persist.
struct ModelInput {
    alias: String,
    provider: String,
    model_id: String,
    max_tokens: Option<u32>,
}
```

Add the same field:

```
/// A model alias to persist.
struct ModelInput {
    alias: String,
    provider: String,
    model_id: String,
    max_tokens: Option<u32>,
    context_window: Option<u32>,
}
```

- [ ] **Step 3: Confirm the workspace fails to compile**

Run: `cargo build -p horsie-server 2>&1 | head -30`
Expected: FAIL — `ModelInput`/`ModelView` construction sites in `store.rs` and its test module are missing the new field.

- [ ] **Step 4: `ModelRow`, the built-in defaults table, and the read/write paths**

`server/src/config/store.rs:543-548` currently reads:

```rust
struct ModelRow {
    alias: String,
    provider: String,
    model_id: String,
    max_tokens: Option<i64>,
}
```

Add the field:

```rust
struct ModelRow {
    alias: String,
    provider: String,
    model_id: String,
    max_tokens: Option<i64>,
    context_window: Option<i64>,
}
```

Add a defaults table near the other `default_*` helpers (`server/src/config/store.rs:581-595`, right after `default_connect_timeout_secs`):

```rust
/// A built-in context-window guess for well-known model ids, applied only
/// when a model is persisted with `context_window` omitted — the stored
/// value (once set) is always authoritative, this never overrides it.
/// Matched by substring against `model_id`; extend as new families ship.
fn default_context_window(model_id: &str) -> Option<u32> {
    const TABLE: &[(&str, u32)] = &[
        ("claude-", 200_000),
        ("gpt-4o", 128_000),
        ("gpt-4.1", 1_000_000),
        ("o1", 200_000),
        ("o3", 200_000),
        ("deepseek", 128_000),
    ];
    TABLE
        .iter()
        .find(|(needle, _)| model_id.contains(needle))
        .map(|(_, window)| *window)
}
```

`server/src/config/store.rs:395-404` currently reads:

```rust
                sqlx::query(
                    "INSERT INTO models (alias, provider, model_id, max_tokens) VALUES (?, ?, ?, ?)",
                )
                .bind(alias)
                .bind(&m.provider)
                .bind(m.model_id.trim())
                .bind(m.max_tokens.map(i64::from))
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
```

Change to:

```rust
                let context_window = m
                    .context_window
                    .or_else(|| default_context_window(m.model_id.trim()));
                sqlx::query(
                    "INSERT INTO models (alias, provider, model_id, max_tokens, context_window) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(alias)
                .bind(&m.provider)
                .bind(m.model_id.trim())
                .bind(m.max_tokens.map(i64::from))
                .bind(context_window.map(i64::from))
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
```

`server/src/config/store.rs:958-975` (`read_models`) currently reads:

```rust
async fn read_models<'e, E>(ex: E) -> Result<Vec<ModelRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let rows =
        sqlx::query("SELECT alias, provider, model_id, max_tokens FROM models ORDER BY alias")
            .fetch_all(ex)
            .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(ModelRow {
            alias: r.try_get("alias")?,
            provider: r.try_get("provider")?,
            model_id: r.try_get("model_id")?,
            max_tokens: r.try_get("max_tokens")?,
        });
    }
    Ok(out)
}
```

Change to:

```rust
async fn read_models<'e, E>(ex: E) -> Result<Vec<ModelRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let rows = sqlx::query(
        "SELECT alias, provider, model_id, max_tokens, context_window FROM models ORDER BY alias",
    )
    .fetch_all(ex)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(ModelRow {
            alias: r.try_get("alias")?,
            provider: r.try_get("provider")?,
            model_id: r.try_get("model_id")?,
            max_tokens: r.try_get("max_tokens")?,
            context_window: r.try_get("context_window")?,
        });
    }
    Ok(out)
}
```

`server/src/config/store.rs:888-895` (`model_view`) currently reads:

```rust
fn model_view(r: &ModelRow) -> ModelView {
    ModelView {
        alias: r.alias.clone(),
        provider: r.provider.clone(),
        model_id: r.model_id.clone(),
        max_tokens: r.max_tokens.and_then(|v| u32::try_from(v).ok()),
    }
}
```

Change to:

```rust
fn model_view(r: &ModelRow) -> ModelView {
    ModelView {
        alias: r.alias.clone(),
        provider: r.provider.clone(),
        model_id: r.model_id.clone(),
        max_tokens: r.max_tokens.and_then(|v| u32::try_from(v).ok()),
        context_window: r.context_window.and_then(|v| u32::try_from(v).ok()),
    }
}
```

- [ ] **Step 5: Fix the test helper**

`server/src/config/store.rs`'s test `mod tests` has, around line 1050:

```rust
    fn model(alias: &str, provider: &str) -> ModelInput {
        ModelInput {
            alias: alias.into(),
            provider: provider.into(),
            model_id: "id".into(),
            max_tokens: None,
        }
    }
```

Change to:

```rust
    fn model(alias: &str, provider: &str) -> ModelInput {
        ModelInput {
            alias: alias.into(),
            provider: provider.into(),
            model_id: "id".into(),
            max_tokens: None,
            context_window: None,
        }
    }
```

- [ ] **Step 6: Build to confirm it's green**

Run: `cargo build -p horsie-server --all-features`
Expected: PASS, no errors.

- [ ] **Step 7: Write the failing test for the default table + override behavior**

Add to `server/src/config/store.rs`'s `mod tests`, near `update_persists_and_swaps_registry`:

```rust
    #[tokio::test]
    async fn context_window_defaults_for_known_models_and_stays_editable() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-inline"))]),
                models: Some(vec![
                    ModelInput {
                        alias: "sonnet".into(),
                        provider: "p".into(),
                        model_id: "claude-sonnet-4-6".into(),
                        max_tokens: None,
                        context_window: None,
                    },
                    ModelInput {
                        alias: "custom".into(),
                        provider: "p".into(),
                        model_id: "some-unknown-model".into(),
                        max_tokens: None,
                        context_window: Some(42_000),
                    },
                ]),
                vendors: None,
                default_vendor: None,
            })
            .await
            .expect("update ok");
        let sonnet = view.models.iter().find(|m| m.alias == "sonnet").unwrap();
        assert_eq!(sonnet.context_window, Some(200_000));
        let custom = view.models.iter().find(|m| m.alias == "custom").unwrap();
        assert_eq!(
            custom.context_window,
            Some(42_000),
            "an explicit value must never be overridden by the default table"
        );
    }
```

- [ ] **Step 8: Run it**

Run: `cargo test -p horsie-server context_window_defaults_for_known_models_and_stays_editable`
Expected: PASS.

- [ ] **Step 9: Run the full server test suite**

Run: `cargo test -p horsie-server`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add server/migrations/0007_model_context_window.sql models/fluorite/settings.fl server/src/config/store.rs
git commit -m "server: add context_window to configured models, with built-in defaults"
```

---

### Task 6: Settings UI — `contextWindow` field on the model row

**Files:**
- Modify: `clients/web/src/pages/SettingsPage.tsx:57-62,89-95,192-194,226,387,830-843` (exact line numbers shift once regenerated types land — locate by the `maxTokens`/`ModelDraft` symbols)

**Interfaces:**
- Consumes: `ModelView.contextWindow?: number`, `ModelInput.contextWindow?: number` (regenerated from Task 5's schema change).

- [ ] **Step 1: Regenerate types**

Run: `cd clients/web && bun install && bun run generate-types`
Expected: `clients/web/src/generated/settings/modelView.ts` and `modelInput.ts` now have a `contextWindow?: number` field.

- [ ] **Step 2: Extend `ModelDraft`**

`clients/web/src/pages/SettingsPage.tsx:57-62` currently reads:

```tsx
type ModelDraft = {
  alias: string;
  provider: string;
  modelId: string;
  maxTokens: string; // "" = unset
};
```

Change to:

```tsx
type ModelDraft = {
  alias: string;
  provider: string;
  modelId: string;
  maxTokens: string; // "" = unset
  contextWindow: string; // "" = unset (server applies a built-in default)
};
```

- [ ] **Step 3: Thread it through `toModelDrafts`**

`clients/web/src/pages/SettingsPage.tsx:89-95` currently reads:

```tsx
const toModelDrafts = (v: SettingsView): ModelDraft[] =>
  v.models.map((m) => ({
    alias: m.alias,
    provider: m.provider,
    modelId: m.modelId,
    maxTokens: m.maxTokens != null ? String(m.maxTokens) : "",
  }));
```

Change to:

```tsx
const toModelDrafts = (v: SettingsView): ModelDraft[] =>
  v.models.map((m) => ({
    alias: m.alias,
    provider: m.provider,
    modelId: m.modelId,
    maxTokens: m.maxTokens != null ? String(m.maxTokens) : "",
    contextWindow: m.contextWindow != null ? String(m.contextWindow) : "",
  }));
```

- [ ] **Step 4: Validate and submit it**

`clients/web/src/pages/SettingsPage.tsx:192-194` currently reads:

```tsx
    for (const m of models)
      if (m.maxTokens.trim() && !/^\d+$/.test(m.maxTokens.trim()))
        return setLocalError(`Max tokens for "${m.alias}" must be a number.`);
```

Add a matching check directly after it:

```tsx
    for (const m of models)
      if (m.maxTokens.trim() && !/^\d+$/.test(m.maxTokens.trim()))
        return setLocalError(`Max tokens for "${m.alias}" must be a number.`);
    for (const m of models)
      if (m.contextWindow.trim() && !/^\d+$/.test(m.contextWindow.trim()))
        return setLocalError(`Context window for "${m.alias}" must be a number.`);
```

`clients/web/src/pages/SettingsPage.tsx:226` (inside the `modelInputs` map) currently reads:

```tsx
    const modelInputs: ModelInput[] = models.map((m) => ({
      alias: m.alias.trim(),
      provider: m.provider,
      modelId: m.modelId.trim(),
      maxTokens: m.maxTokens.trim() ? Number(m.maxTokens.trim()) : undefined,
    }));
```

Change to:

```tsx
    const modelInputs: ModelInput[] = models.map((m) => ({
      alias: m.alias.trim(),
      provider: m.provider,
      modelId: m.modelId.trim(),
      maxTokens: m.maxTokens.trim() ? Number(m.maxTokens.trim()) : undefined,
      contextWindow: m.contextWindow.trim() ? Number(m.contextWindow.trim()) : undefined,
    }));
```

- [ ] **Step 5: The "add model" default draft**

`clients/web/src/pages/SettingsPage.tsx:387` currently reads:

```tsx
                    { alias: "", provider: providerNames[0] ?? "", modelId: "", maxTokens: "" },
```

Change to:

```tsx
                    {
                      alias: "",
                      provider: providerNames[0] ?? "",
                      modelId: "",
                      maxTokens: "",
                      contextWindow: "",
                    },
```

- [ ] **Step 6: The form field itself**

In the `ModelRow` component (around `clients/web/src/pages/SettingsPage.tsx:830-843`), the grid currently ends with:

```tsx
        <TextField
          label="Max tokens (optional)"
          value={draft.maxTokens}
          onChange={(v) => set({ maxTokens: v })}
          placeholder="8192"
        />
      </div>
    </RowShell>
  );
}
```

Change to:

```tsx
        <TextField
          label="Max tokens (optional)"
          value={draft.maxTokens}
          onChange={(v) => set({ maxTokens: v })}
          placeholder="8192"
        />
        <TextField
          label="Context window (optional)"
          value={draft.contextWindow}
          onChange={(v) => set({ contextWindow: v })}
          placeholder="200000"
        />
      </div>
    </RowShell>
  );
}
```

- [ ] **Step 7: Type-check**

Run: `cd clients/web && bun run typecheck`
Expected: PASS, no type errors.

- [ ] **Step 8: Commit**

```bash
git add clients/web/src/pages/SettingsPage.tsx clients/web/src/generated
git commit -m "web: add context window field to the model settings row"
```

---

### Task 7: `SessionStats` schema and the journal fold

**Files:**
- Modify: `models/fluorite/session.fl` (new `SessionStats` struct)
- Modify: `models/fluorite/session_api.fl` (new `GetSessionStatsResponse`)
- Modify: `server/src/sessions/events.rs` (new `fold_session_usage`)
- Test: `server/src/sessions/events.rs` (extend the existing `mod tests`)

**Interfaces:**
- Produces: `pub async fn fold_session_usage(journal: &Arc<dyn Journal>, session_id: Uuid) -> SessionUsageFold`, where `SessionUsageFold { current: Usage, total: Usage, turn_count: u32 }` — consumed by Task 8's HTTP handler.
- Produces: `horsie_models::session::SessionStats { model: String, context_window: Option<u32>, current: Usage, total: Usage, turn_count: u32 }` and `horsie_models::session_api::GetSessionStatsResponse { stats: SessionStats }`.

- [ ] **Step 1: Add the wire types**

`models/fluorite/session.fl` — add near `TurnCompletedEvent` (after line 60):

```
/// Snapshot returned by `GET /api/sessions/:id/stats`.
struct SessionStats {
    model: String,
    /// The model's configured context window, when known.
    context_window: Option<u32>,
    /// Usage from the most recently completed turn — what's actually loaded
    /// in the model's context right now.
    current: Usage,
    /// Usage summed across every completed turn in the session.
    total: Usage,
    turn_count: u32,
}
```

`models/fluorite/session_api.fl` — add next to `GetSessionResponse` (line 37):

```
struct GetSessionStatsResponse { stats: SessionStats }
```

- [ ] **Step 2: Regenerate Rust types and confirm they exist**

Run: `cargo build -p horsie-models`
Expected: PASS — `horsie_models::session::SessionStats` and `horsie_models::session_api::GetSessionStatsResponse` now exist (fluorite codegen runs at build time via `models/build.rs`).

- [ ] **Step 3: Write the failing fold test**

Add to `server/src/sessions/events.rs`'s `mod tests`, near `fold_session_state_reads_pending_question`:

```rust
    #[tokio::test]
    async fn fold_session_usage_tracks_current_and_total() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let sid = Uuid::new_v4();
        let pid = AgentActor::persistence_id_for(sid);

        // No completed turns yet: an all-zero, cache-less snapshot.
        let fold = fold_session_usage(&journal, sid).await;
        assert_eq!(fold.current.input_tokens, 0);
        assert_eq!(fold.total.input_tokens, 0);
        assert_eq!(fold.turn_count, 0);

        let events = vec![
            serde_json::to_vec(&AgentDomainEvent::RunComplete {
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_creation_tokens: Some(3),
                    cache_read_tokens: None,
                },
                iterations: 1,
            })
            .unwrap(),
            serde_json::to_vec(&AgentDomainEvent::RunComplete {
                usage: Usage {
                    input_tokens: 40,
                    output_tokens: 8,
                    cache_creation_tokens: None,
                    cache_read_tokens: Some(30),
                },
                iterations: 1,
            })
            .unwrap(),
        ];
        journal.persist(&pid, &events).await.unwrap();

        let fold = fold_session_usage(&journal, sid).await;
        // "current" is the *last* turn only.
        assert_eq!(fold.current.input_tokens, 40);
        assert_eq!(fold.current.cache_read_tokens, Some(30));
        assert_eq!(fold.current.cache_creation_tokens, None);
        // "total" sums both turns; a field present on only one turn still sums.
        assert_eq!(fold.total.input_tokens, 50);
        assert_eq!(fold.total.output_tokens, 13);
        assert_eq!(fold.total.cache_creation_tokens, Some(3));
        assert_eq!(fold.total.cache_read_tokens, Some(30));
        assert_eq!(fold.turn_count, 2);
    }
```

- [ ] **Step 4: Run it to see it fail**

Run: `cargo test -p horsie-server fold_session_usage_tracks_current_and_total`
Expected: FAIL — `fold_session_usage` doesn't exist yet.

- [ ] **Step 5: Implement the fold**

Add to `server/src/sessions/events.rs`, right after `fold_session_state` (after line 154):

```rust
/// Folded [`Usage`] for `GET /api/sessions/:id/stats`.
pub struct SessionUsageFold {
    /// Usage from the most recently completed turn.
    pub current: Usage,
    /// Usage summed across every completed turn.
    pub total: Usage,
    pub turn_count: u32,
}

impl Default for SessionUsageFold {
    fn default() -> Self {
        Self {
            current: Usage::without_cache(0, 0),
            total: Usage::without_cache(0, 0),
            turn_count: 0,
        }
    }
}

/// Fold a session's own agent journal into cumulative + most-recent [`Usage`].
/// Mirrors `fold_session_state`'s replay-from-0 shape: interactive agents
/// never compact, so this always sees the full history.
pub async fn fold_session_usage(journal: &Arc<dyn Journal>, session_id: Uuid) -> SessionUsageFold {
    let pid = AgentActor::persistence_id_for(session_id);
    let mut fold = SessionUsageFold::default();
    let mut stream = journal.replay(&pid, 0).await;
    while let Some(item) = stream.next().await {
        let Ok(bytes) = item else { break };
        if let Ok(AgentDomainEvent::RunComplete { usage, .. }) =
            serde_json::from_slice::<AgentDomainEvent>(&bytes)
        {
            fold.total.input_tokens += usage.input_tokens;
            fold.total.output_tokens += usage.output_tokens;
            fold.total.cache_creation_tokens =
                sum_optional(fold.total.cache_creation_tokens, usage.cache_creation_tokens);
            fold.total.cache_read_tokens =
                sum_optional(fold.total.cache_read_tokens, usage.cache_read_tokens);
            fold.current = usage;
            fold.turn_count += 1;
        }
    }
    fold
}

/// Sums two optional per-turn cache-token counts. Stays `None` only when
/// *neither* side reported anything.
fn sum_optional(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}
```

Add `Usage` to the existing `horsie_models::session::{...}` import at the top of `server/src/sessions/events.rs:14`:

```rust
use horsie_models::session::{MessageEvent, SessionEvent, ToolOutputEvent, TurnCompletedEvent};
```

becomes

```rust
use horsie_models::agent::Usage;
use horsie_models::session::{MessageEvent, SessionEvent, ToolOutputEvent, TurnCompletedEvent};
```

- [ ] **Step 6: Run the test again**

Run: `cargo test -p horsie-server fold_session_usage_tracks_current_and_total`
Expected: PASS.

- [ ] **Step 7: Run the full server test suite**

Run: `cargo test -p horsie-server`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add models/fluorite/session.fl models/fluorite/session_api.fl server/src/sessions/events.rs
git commit -m "server: add SessionStats schema and fold_session_usage"
```

---

### Task 8: `GET /api/sessions/:id/stats`

**Files:**
- Modify: `server/src/http/mod.rs:87-90` (route table)
- Modify: `server/src/http/handlers.rs` (new `get_session_stats` handler)
- Test: `server/src/http/mod.rs` (extend `mod tests`)

**Interfaces:**
- Consumes: `fold_session_usage` (Task 7), `state.config_store.view()` (existing `ConfigStore` trait), `SessionSupervisorCommand::Get` (existing).
- Produces: `GET /api/sessions/:id/stats` → `200 Json<GetSessionStatsResponse>` | `404` (unknown session id).

- [ ] **Step 1: Write the failing HTTP test**

Add to `server/src/http/mod.rs`'s `mod tests`, near `create_with_repos_builds_provision_steps`:

```rust
    #[tokio::test]
    async fn session_stats_endpoint_round_trips() {
        use horsie_models::session_api::GetSessionStatsResponse;
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let body = serde_json::json!({
            "agent": {"model": "mock"},
            "vendor": "mock"
        });
        let res = app
            .clone()
            .oneshot(post_json("/api/sessions", &body))
            .await
            .unwrap();
        let created: CreateSessionResponse = read_json(res).await;
        let id = created.session.id;

        // Fresh session: zeroed, cache-less stats, model carried through.
        let res = app
            .clone()
            .oneshot(get(&format!("/api/sessions/{id}/stats")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let stats: GetSessionStatsResponse = read_json(res).await;
        assert_eq!(stats.stats.model, "mock");
        assert_eq!(stats.stats.current.input_tokens, 0);
        assert_eq!(stats.stats.total.input_tokens, 0);
        assert_eq!(stats.stats.turn_count, 0);
        assert_eq!(stats.stats.context_window, None);

        // Unknown session -> 404.
        let res = app
            .oneshot(get("/api/sessions/00000000-0000-0000-0000-000000000000/stats"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p horsie-server session_stats_endpoint_round_trips`
Expected: FAIL — `404` for the new route (not registered yet).

- [ ] **Step 3: Add the handler**

Add to `server/src/http/handlers.rs`, right after `get_session` (after line 193):

```rust
pub async fn get_session_stats(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Api> {
    let rec = ask(&state, |reply| SessionSupervisorCommand::Get {
        id: id.clone(),
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such session: {id}")))?;
    let uuid =
        Uuid::parse_str(&id).map_err(|_| Api::not_found(format!("no such session: {id}")))?;

    let fold = fold_session_usage(&state.journal, uuid).await;
    let view = state.config_store.view().await.map_err(Api::internal)?;
    let context_window = view
        .models
        .iter()
        .find(|m| m.alias == rec.spec.agent.model)
        .and_then(|m| m.context_window);

    let stats = SessionStats {
        model: rec.spec.agent.model.clone(),
        context_window,
        current: fold.current,
        total: fold.total,
        turn_count: fold.turn_count,
    };
    Ok(Json(GetSessionStatsResponse { stats }))
}
```

Update `server/src/http/handlers.rs`'s imports (lines 7, 17-23):

```rust
use crate::sessions::events::fold_session_state;
```

becomes

```rust
use crate::sessions::events::{fold_session_state, fold_session_usage};
```

and

```rust
use horsie_models::session::{
    AgentSettings as WireAgentSettings, SessionDetail, SessionStatusKind, SessionSummary,
};
use horsie_models::session_api::{
    CreateSessionRequest, CreateSessionResponse, GetSessionResponse, ListSessionsResponse,
    SendMessageRequest, SessionAck,
};
```

becomes

```rust
use horsie_models::session::{
    AgentSettings as WireAgentSettings, SessionDetail, SessionStats, SessionStatusKind,
    SessionSummary,
};
use horsie_models::session_api::{
    CreateSessionRequest, CreateSessionResponse, GetSessionResponse, GetSessionStatsResponse,
    ListSessionsResponse, SendMessageRequest, SessionAck,
};
```

- [ ] **Step 4: Register the route**

`server/src/http/mod.rs:87-90` currently reads:

```rust
        .route(
            "/api/sessions/:id",
            get(handlers::get_session).delete(handlers::delete_session),
        )
```

Add a new route right after it:

```rust
        .route(
            "/api/sessions/:id",
            get(handlers::get_session).delete(handlers::delete_session),
        )
        .route("/api/sessions/:id/stats", get(handlers::get_session_stats))
```

- [ ] **Step 5: Run the test again**

Run: `cargo test -p horsie-server session_stats_endpoint_round_trips`
Expected: PASS.

- [ ] **Step 6: Run the full server test suite**

Run: `cargo test -p horsie-server`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add server/src/http/handlers.rs server/src/http/mod.rs
git commit -m "server: add GET /api/sessions/:id/stats"
```

---

### Task 9: Web — API client, `useSessionStats`, SSE-driven invalidation

**Files:**
- Modify: `clients/web/src/api/client.ts` (`api.sessions.stats`)
- Modify: `clients/web/src/hooks/useSessions.ts` (`qk.sessionStats`, `useSessionStats`)
- Modify: `clients/web/src/hooks/useSessionStream.ts` (invalidate on `TurnCompleted`)

**Interfaces:**
- Produces: `api.sessions.stats(id): Promise<GetSessionStatsResponse>`, `qk.sessionStats(id): readonly ["session-stats", string]`, `useSessionStats(id: string | undefined): UseQueryResult<SessionStats>`.
- Consumes: regenerated `SessionStats`/`GetSessionStatsResponse` types (from Task 7's schema change — regenerate first).

- [ ] **Step 1: Regenerate types**

Run: `cd clients/web && bun run generate-types`
Expected: `clients/web/src/generated/session/sessionStats.ts` and `clients/web/src/generated/session_api/getSessionStatsResponse.ts` now exist, re-exported automatically via `clients/web/src/api/types.ts`'s existing `export * from "../generated/session"` / `"../generated/session_api"` (no edit needed there).

- [ ] **Step 2: Add the API client method**

`clients/web/src/api/client.ts`'s `sessions` object currently ends its `get`/`stop` block with:

```ts
    stop: (id: string): Promise<SessionAck> =>
      request(`/sessions/${encodeURIComponent(id)}/stop`, {
        method: "POST",
        body: "{}",
      }),
  },
```

Add a `stats` method:

```ts
    stop: (id: string): Promise<SessionAck> =>
      request(`/sessions/${encodeURIComponent(id)}/stop`, {
        method: "POST",
        body: "{}",
      }),

    stats: (id: string): Promise<GetSessionStatsResponse> =>
      request(`/sessions/${encodeURIComponent(id)}/stats`),
  },
```

Add `GetSessionStatsResponse` to the `import type { ... } from "./types"` block at the top of the file (alongside the existing `GetSessionResponse`).

- [ ] **Step 3: Add the query key and hook**

`clients/web/src/hooks/useSessions.ts:18-20` currently reads:

```ts
export const qk = {
  sessions: ["sessions"] as const,
  session: (id: string) => ["session", id] as const,
};
```

Change to:

```ts
export const qk = {
  sessions: ["sessions"] as const,
  session: (id: string) => ["session", id] as const,
  sessionStats: (id: string) => ["session-stats", id] as const,
};
```

Add a new hook right after `useSession` (after line 36):

```ts
export function useSessionStats(id: string | undefined) {
  return useQuery({
    queryKey: id ? qk.sessionStats(id) : ["session-stats", "none"],
    queryFn: () => api.sessions.stats(id as string),
    enabled: !!id,
    select: (r: GetSessionStatsResponse) => r.stats,
  });
}
```

Add `GetSessionStatsResponse` to the `import type { ... } from "../api/types"` block at the top of the file.

- [ ] **Step 4: Invalidate on `TurnCompleted`**

`clients/web/src/hooks/useSessionStream.ts:267-289` (the connection `useEffect`) currently reads:

```ts
  useEffect(() => {
    dispatch({ kind: "reset" });
    if (!sessionId) return;

    const es = new EventSource(api.sessionEventsUrl(sessionId));
    esRef.current = es;

    es.onopen = () => dispatch({ kind: "connected", value: true });
    es.onmessage = (e: MessageEvent<string>) => {
      try {
        const event = JSON.parse(e.data) as SessionEvent;
        dispatch({ kind: "event", event });
      } catch (err) {
        console.error("failed to parse session event", err, e.data);
      }
    };
    es.onerror = () => dispatch({ kind: "connected", value: false });

    return () => {
      es.close();
      esRef.current = null;
    };
  }, [sessionId]);
```

Change to:

```ts
  const queryClient = useQueryClient();

  useEffect(() => {
    dispatch({ kind: "reset" });
    if (!sessionId) return;

    const es = new EventSource(api.sessionEventsUrl(sessionId));
    esRef.current = es;

    es.onopen = () => dispatch({ kind: "connected", value: true });
    es.onmessage = (e: MessageEvent<string>) => {
      try {
        const event = JSON.parse(e.data) as SessionEvent;
        dispatch({ kind: "event", event });
        if (event.type === "TurnCompleted") {
          queryClient.invalidateQueries({ queryKey: qk.sessionStats(sessionId) });
        }
      } catch (err) {
        console.error("failed to parse session event", err, e.data);
      }
    };
    es.onerror = () => dispatch({ kind: "connected", value: false });

    return () => {
      es.close();
      esRef.current = null;
    };
  }, [sessionId, queryClient]);
```

Add these imports at the top of `clients/web/src/hooks/useSessionStream.ts`:

```ts
import { useQueryClient } from "@tanstack/react-query";
import { qk } from "./useSessions";
```

- [ ] **Step 5: Type-check**

Run: `cd clients/web && bun run typecheck`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add clients/web/src/api/client.ts clients/web/src/hooks/useSessions.ts clients/web/src/hooks/useSessionStream.ts clients/web/src/generated
git commit -m "web: add session stats API client, hook, and SSE-driven invalidation"
```

---

### Task 10: Web — `ContextStatsPanel` popover, wired into the session header

**Files:**
- Create: `clients/web/src/components/ContextStatsPanel.tsx`
- Modify: `clients/web/src/pages/SessionView.tsx:1-20,157-164`

**Interfaces:**
- Consumes: `useSessionStats(id)` (Task 9), `SessionStats`/`Usage` generated types, existing `compactNumber` (`clients/web/src/lib/format.ts:24-28`).
- Produces: `<ContextStatsPanel stats={...} totalTokens={...} />`, replacing the header's plain `Gauge` `Chip`.

- [ ] **Step 1: Create the component**

Create `clients/web/src/components/ContextStatsPanel.tsx`:

```tsx
import { Gauge } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { Usage } from "../api/types";
import { compactNumber } from "../lib/format";

function StatRow({
  label,
  value,
  hint,
}: {
  label: string;
  value: string;
  hint: string;
}) {
  return (
    <div
      className="flex items-baseline justify-between gap-3 py-0.5"
      title={hint}
    >
      <span className="text-xs text-muted">{label}</span>
      <span className="font-mono text-xs text-text">{value}</span>
    </div>
  );
}

function UsageBreakdown({ usage }: { usage: Usage }) {
  return (
    <>
      <StatRow
        label="Input"
        value={compactNumber(usage.inputTokens)}
        hint="Full prompt sent this turn: system prompt, tool definitions, and the conversation history so far. This is what counts against the context window."
      />
      <StatRow
        label="Output"
        value={compactNumber(usage.outputTokens)}
        hint="Tokens the model generated back this turn."
      />
      {usage.cacheReadTokens != null && (
        <StatRow
          label="Cache read"
          value={compactNumber(usage.cacheReadTokens)}
          hint="Served from the provider's prompt cache at a steep discount, instead of being reprocessed at full price."
        />
      )}
      {usage.cacheCreationTokens != null && (
        <StatRow
          label="Cache write"
          value={compactNumber(usage.cacheCreationTokens)}
          hint="Written to the provider's prompt cache this turn at a premium — pays off as cache reads on later turns that reuse it."
        />
      )}
    </>
  );
}

export function ContextStatsPanel({
  stats,
  totalTokens,
}: {
  stats: import("../api/types").SessionStats | undefined;
  totalTokens: number;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [open]);

  if (totalTokens <= 0) return null;

  const fillPct =
    stats?.contextWindow != null
      ? Math.min(100, Math.round((stats.current.inputTokens / stats.contextWindow) * 100))
      : null;

  return (
    <div className="relative" ref={ref}>
      <button
        className="chip hover:bg-surface-3"
        onClick={() => setOpen((o) => !o)}
        title={
          stats
            ? `${stats.current.inputTokens} in this turn · ${totalTokens} total`
            : undefined
        }
        data-testid="context-stats-button"
      >
        <Gauge size={12} />
        {compactNumber(totalTokens)} tok
      </button>
      {open && stats && (
        <div
          className="card absolute left-0 top-full z-10 mt-1.5 w-72 p-3 shadow-lg"
          data-testid="context-stats-panel"
        >
          <div className="mb-2">
            <div
              className="flex items-center justify-between text-xs text-muted"
              title="Tokens currently loaded in the model's context, out of its context window. Cache status doesn't shrink this — it only affects price and speed."
            >
              <span>Context window</span>
              <span className="font-mono">
                {compactNumber(stats.current.inputTokens)}
                {stats.contextWindow != null &&
                  ` / ${compactNumber(stats.contextWindow)}`}
              </span>
            </div>
            {fillPct != null && (
              <div className="mt-1 h-1.5 w-full rounded-full bg-surface-2">
                <div
                  className="h-1.5 rounded-full bg-accent"
                  style={{ width: `${fillPct}%` }}
                />
              </div>
            )}
          </div>

          <div className="mb-1 text-[11px] font-semibold uppercase text-faint">
            This turn
          </div>
          <UsageBreakdown usage={stats.current} />

          <div className="mt-2 mb-1 text-[11px] font-semibold uppercase text-faint">
            Session total ({stats.turnCount} {stats.turnCount === 1 ? "turn" : "turns"})
          </div>
          <UsageBreakdown usage={stats.total} />
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 2: Wire it into the session header**

`clients/web/src/pages/SessionView.tsx:157-164` currently reads:

```tsx
            {totalTokens > 0 && (
              <Chip
                icon={<Gauge size={12} />}
                title={`${stream.usage.input} in · ${stream.usage.output} out`}
              >
                {compactNumber(totalTokens)} tok
              </Chip>
            )}
```

Change to:

```tsx
            {totalTokens > 0 && (
              <ContextStatsPanel stats={stats} totalTokens={totalTokens} />
            )}
```

Add the hook call near the existing `useSessionStream` call (`clients/web/src/pages/SessionView.tsx:51`):

```tsx
  const { stream, addOptimisticUser, removeOptimisticUser } = useSessionStream(id);
```

becomes

```tsx
  const { stream, addOptimisticUser, removeOptimisticUser } = useSessionStream(id);
  const { data: stats } = useSessionStats(id);
```

Add imports at the top of `clients/web/src/pages/SessionView.tsx`:

```tsx
import { ContextStatsPanel } from "../components/ContextStatsPanel";
```

and add `useSessionStats` to the existing `import { useDeleteSession, useSendMessage, useSession, useStopSession } from "../hooks/useSessions";` block (`clients/web/src/pages/SessionView.tsx:21-26`):

```tsx
import {
  useDeleteSession,
  useSendMessage,
  useSession,
  useSessionStats,
  useStopSession,
} from "../hooks/useSessions";
```

`Gauge` is no longer used directly in `SessionView.tsx` once the inline `Chip` is replaced — remove it from the `lucide-react` import list at the top of the file if no other usage remains in that file (check with `grep -n "Gauge" clients/web/src/pages/SessionView.tsx` after the edit).

- [ ] **Step 3: Type-check**

Run: `cd clients/web && bun run typecheck`
Expected: PASS.

- [ ] **Step 4: Manual smoke test**

Run: `make web` from the repo root (starts the dev server against a running `horsie serve` backend, per `Makefile:63-64`).
In the browser: open a session with at least one completed turn, confirm the token chip in the header still shows the compact total, click it, and confirm the popover opens showing "This turn" and "Session total" breakdowns with hover tooltips on each row, and a context-window bar when the model has a `context_window` configured (set one via the Settings page model row from Task 6 if none is configured yet). Click outside the popover and confirm it closes.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/components/ContextStatsPanel.tsx clients/web/src/pages/SessionView.tsx
git commit -m "web: expand the session header token chip into a context/cache stats popover"
```

---

### Task 11: Final verification and PR

**Files:** none (verification only)

- [ ] **Step 1: Full workspace check**

Run: `make check` (runs `fmt-check clippy test` per `Makefile:55`)
Expected: PASS, no warnings.

- [ ] **Step 2: Web build**

Run: `make web-build`
Expected: PASS — `clients/web` builds and type-checks against the regenerated types.

- [ ] **Step 3: `clients/ts` codegen stays in sync**

Run: `cd clients/ts && npm install --no-audit --no-fund && npm run generate-types && npm run typecheck`
Expected: PASS (per `Makefile:57-60` — this is the CI drift check for the standalone TS package).

- [ ] **Step 4: Push and open the PR**

```bash
git push -u origin worktree-horsie-session-stats
gh pr create --title "Add session context/token stats widget" --body "$(cat <<'EOF'
## Summary
- `Usage` gains optional cache-creation/cache-read token fields, populated by both the Anthropic and OpenAI-compatible providers from data they already receive.
- Configured models get an optional `context_window`, with a built-in default for common model ids.
- New `GET /api/sessions/:id/stats`, computed by folding the session's existing journal (no new persistence).
- The session header's token chip expands into a popover: context-window fullness, current-turn and session-total breakdowns, each with an inline explanation, kept live via SSE-driven query invalidation.

Design: `docs/superpowers/specs/2026-07-21-session-context-stats-design.md`
Plan: `docs/superpowers/plans/2026-07-21-session-context-stats.md`

## Test plan
- [ ] `make check` green
- [ ] `make web-build` green
- [ ] Manual: open a session, complete a turn, confirm the popover shows current + total usage with tooltips and (when a model has `context_window` set) a fill bar
EOF
)"
```

Expected: PR created against `main`; report the PR URL.
