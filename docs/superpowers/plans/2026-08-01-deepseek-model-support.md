# DeepSeek Model Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `deepseek-v4-flash` and `deepseek-v4-pro` first-class horsie models — correct catalog entries, working thinking control, and forced tool calls that do not 400 — and give model cards a `base_url` column.

**Architecture:** DeepSeek rides the existing `kind = "openai"` provider; no new provider kind and no new thinking dialect. One new per-model capability flag, `forced_tools_disable_thinking`, travels from the model card through the `models` table into `OpenAiProvider`, where it forces `reasoning_effort: "none"` on exactly those requests that pin a tool. Model cards additionally gain a `base_url` column that is stored and admin-editable but read by nothing.

**Tech Stack:** Rust (axum, sqlx/SQLite, reqwest), fluorite schema codegen, React + TypeScript (bun, Playwright).

**Spec:** `docs/superpowers/specs/2026-08-01-deepseek-model-support-design.md`

## Global Constraints

- Work in the existing worktree `.horsie/worktrees/deepseek-models` on branch `feat/deepseek-models`.
- Card values for **both** `deepseek-v4-flash` and `deepseek-v4-pro` are identical and probe-verified — use these exact numbers: `base_url` `https://api.deepseek.com`, `context_window` `1048576`, `max_tokens` `393216`, `thinking_dialect` `openai_effort`, `thinking_efforts` `["none","minimal","low","medium","high","xhigh","max"]`, `default_thinking_effort` `high`, `forced_tools_disable_thinking` true.
- Display names: `DeepSeek V4 Flash` and `DeepSeek V4 Pro`.
- Never add a `deepseek` provider kind, a `deepseek_thinking` dialect, or any change to cache/usage parsing — all three are explicitly out of scope and already correct.
- Nothing on the server may read a card's `base_url`. Cards stay prefill templates never linked to configured models.
- Rust: run `cargo fmt` **before** `cargo clippy` (clippy fails on unformatted code in this repo).
- Commit after every task.

---

### Task 1: `forced_tools_disable_thinking` in `OpenAiProvider`

The whole behavioural fix, self-contained in one crate with no schema involved.

**Files:**
- Modify: `providers/openai/src/lib.rs` (struct at ~108, `build`, `build_body` at ~185, builders at ~240, tests at ~771)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: `OpenAiProvider::with_forced_tools_disable_thinking(self, bool) -> Self`, consumed by Task 4's `build_openai`.

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `providers/openai/src/lib.rs`, just before its closing brace:

```rust
    fn tool_spec() -> horsie_agentcore::ToolSpec {
        horsie_agentcore::ToolSpec {
            name: "get_weather".into(),
            description: "Get the weather.".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    /// DeepSeek rejects a pinned tool while thinking is on
    /// ("Thinking mode does not support this tool_choice"), so a model card
    /// carrying this flag must turn thinking off for exactly those requests.
    #[test]
    fn forced_tool_choice_disables_thinking_when_flagged() {
        let p = OpenAiProvider::new()
            .expect("builds")
            .with_thinking_dialect(horsie_agentcore::ThinkingDialect::OpenAiEffort)
            .with_forced_tools_disable_thinking(true);
        let msgs: Vec<horsie_models::agent::Message> = vec![];
        for choice in [
            ToolChoice::Any,
            ToolChoice::Required("get_weather".into()),
        ] {
            let req = CompletionRequest {
                messages: &msgs,
                system: None,
                tools: vec![tool_spec()],
                tool_choice: choice.clone(),
                max_tokens: None,
                thinking_effort: horsie_agentcore::ThinkingEffort::parse("high"),
            };
            assert_eq!(
                p.build_body(&req).reasoning_effort.as_deref(),
                Some("none"),
                "{choice:?} must disable thinking",
            );
        }
    }

    /// The flag must fire even when the model declares no thinking control at
    /// all: DeepSeek thinks by default, so sending nothing still 400s.
    #[test]
    fn forced_tool_choice_disables_thinking_even_without_a_dialect() {
        let p = OpenAiProvider::new()
            .expect("builds")
            .with_forced_tools_disable_thinking(true);
        let msgs: Vec<horsie_models::agent::Message> = vec![];
        let req = CompletionRequest {
            messages: &msgs,
            system: None,
            tools: vec![tool_spec()],
            tool_choice: ToolChoice::Any,
            max_tokens: None,
            thinking_effort: None,
        };
        assert_eq!(p.build_body(&req).reasoning_effort.as_deref(), Some("none"));
    }

    #[test]
    fn auto_tool_choice_keeps_thinking_even_when_flagged() {
        let p = OpenAiProvider::new()
            .expect("builds")
            .with_thinking_dialect(horsie_agentcore::ThinkingDialect::OpenAiEffort)
            .with_forced_tools_disable_thinking(true);
        let msgs: Vec<horsie_models::agent::Message> = vec![];
        let req = CompletionRequest {
            messages: &msgs,
            system: None,
            tools: vec![tool_spec()],
            tool_choice: ToolChoice::Auto,
            max_tokens: None,
            thinking_effort: horsie_agentcore::ThinkingEffort::parse("high"),
        };
        assert_eq!(p.build_body(&req).reasoning_effort.as_deref(), Some("high"));
    }

    /// No tools means no `tool_choice` on the wire, so nothing to reconcile.
    #[test]
    fn flag_is_inert_without_tools() {
        let p = OpenAiProvider::new()
            .expect("builds")
            .with_thinking_dialect(horsie_agentcore::ThinkingDialect::OpenAiEffort)
            .with_forced_tools_disable_thinking(true);
        let msgs: Vec<horsie_models::agent::Message> = vec![];
        let req = CompletionRequest {
            messages: &msgs,
            system: None,
            tools: vec![],
            tool_choice: ToolChoice::Any,
            max_tokens: None,
            thinking_effort: horsie_agentcore::ThinkingEffort::parse("high"),
        };
        let body = p.build_body(&req);
        assert!(body.tool_choice.is_none());
        assert_eq!(body.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn forced_tool_choice_keeps_thinking_when_not_flagged() {
        let p = OpenAiProvider::new()
            .expect("builds")
            .with_thinking_dialect(horsie_agentcore::ThinkingDialect::OpenAiEffort);
        let msgs: Vec<horsie_models::agent::Message> = vec![];
        let req = CompletionRequest {
            messages: &msgs,
            system: None,
            tools: vec![tool_spec()],
            tool_choice: ToolChoice::Any,
            max_tokens: None,
            thinking_effort: horsie_agentcore::ThinkingEffort::parse("high"),
        };
        assert_eq!(p.build_body(&req).reasoning_effort.as_deref(), Some("high"));
    }
```

If `ToolChoice` does not derive `Clone`/`Debug`, replace the loop in the first
test with two straight-line blocks rather than adding derives.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p horsie-openai forced_tool 2>&1 | tail -20
```

Expected: compile error — `no method named with_forced_tools_disable_thinking`.

- [ ] **Step 3: Add the field**

In `providers/openai/src/lib.rs`, add to `pub struct OpenAiProvider` after `thinking_dialect`:

```rust
    /// Backends that reject a pinned `tool_choice` while thinking is enabled.
    /// DeepSeek answers `Thinking mode does not support this tool_choice` with
    /// a 400. Per-model data rather than inference, for the same reason
    /// `thinking_dialect` is.
    forced_tools_disable_thinking: bool,
```

and to the `Self { .. }` literal in `fn build`, after `thinking_dialect: ThinkingDialect::NoControl,`:

```rust
            forced_tools_disable_thinking: false,
```

- [ ] **Step 4: Add the builder**

Next to `with_thinking_dialect` in the same `impl OpenAiProvider` block:

```rust
    /// Force `reasoning_effort: "none"` on requests that pin a tool, for
    /// backends that reject the combination.
    #[must_use]
    pub fn with_forced_tools_disable_thinking(mut self, yes: bool) -> Self {
        self.forced_tools_disable_thinking = yes;
        self
    }
```

- [ ] **Step 5: Apply the override in `build_body`**

In `build_body`, between the `let tool_choice = ...` binding and the
`ChatRequest { .. }` literal, insert:

```rust
        // `tool_choice` is `Some` only when tools exist AND the choice is not
        // `Auto` — precisely the shapes DeepSeek rejects under thinking. This
        // sits outside the dialect match on purpose: a model with no thinking
        // control still thinks by default on such a backend, so a
        // dialect-local fix would miss `NoControl` entirely.
        let reasoning_effort = if self.forced_tools_disable_thinking && tool_choice.is_some() {
            Some("none".to_string())
        } else {
            match (self.thinking_dialect, request.thinking_effort) {
                (ThinkingDialect::OpenAiEffort, Some(e)) => Some(e.as_str().to_string()),
                _ => None,
            }
        };
```

Then replace the `reasoning_effort:` field in the `ChatRequest` literal with the
shorthand `reasoning_effort,` and delete the old inline `match`.

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cargo fmt -p horsie-openai
cargo test -p horsie-openai 2>&1 | tail -20
```

Expected: all pass, including the pre-existing
`reasoning_effort_set_for_openai_effort_dialect`.

- [ ] **Step 7: Lint**

```bash
cargo clippy -p horsie-openai --all-targets 2>&1 | tail -20
```

Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add providers/openai/src/lib.rs
git commit -m "feat(openai): disable thinking when a tool choice is pinned"
```

---

### Task 2: Migration — new columns and the DeepSeek catalog rows

SQL only. Runs before any Rust reads the columns, so it is independently
reviewable and independently revertible.

**Files:**
- Create: `server/migrations/0013_deepseek_v4_and_card_base_url.sql`
- Test: `server/src/config/model_cards.rs` (new test in the existing `mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: columns `model_cards.base_url`, `model_cards.forced_tools_disable_thinking`, `models.forced_tools_disable_thinking`; catalog rows `deepseek-v4-flash` and `deepseek-v4-pro`. Tasks 3–5 read these.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `server/src/config/model_cards.rs`:

```rust
    /// The migration must both add the columns and correct the catalog:
    /// seeding is insert-if-missing, so an existing DB can only be fixed here.
    #[tokio::test]
    async fn migration_replaces_deepseek_chat_with_the_v4_cards() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::config::store::open_pool(&format!("sqlite://{}/t.db", dir.path().display()))
            .await
            .unwrap();

        let stale: Option<String> =
            sqlx::query_scalar("SELECT model_id FROM model_cards WHERE model_id = 'deepseek-chat'")
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert!(stale.is_none(), "deepseek-chat must be gone");

        for id in ["deepseek-v4-flash", "deepseek-v4-pro"] {
            let row = sqlx::query(
                "SELECT base_url, context_window, max_tokens, thinking_dialect, \
                 default_thinking_effort, thinking_efforts, forced_tools_disable_thinking \
                 FROM model_cards WHERE model_id = ?",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("{id} must exist: {e}"));

            assert_eq!(
                row.get::<Option<String>, _>("base_url").as_deref(),
                Some("https://api.deepseek.com"),
                "{id} base_url",
            );
            assert_eq!(row.get::<i64, _>("context_window"), 1_048_576, "{id} ctx");
            assert_eq!(row.get::<i64, _>("max_tokens"), 393_216, "{id} max_tokens");
            assert_eq!(
                row.get::<Option<String>, _>("thinking_dialect").as_deref(),
                Some("openai_effort"),
                "{id} dialect",
            );
            assert_eq!(
                row.get::<Option<String>, _>("default_thinking_effort")
                    .as_deref(),
                Some("high"),
                "{id} default effort",
            );
            assert_eq!(
                row.get::<i64, _>("forced_tools_disable_thinking"),
                1,
                "{id} must disable thinking for pinned tools",
            );
            let efforts: Vec<String> = serde_json::from_str(
                &row.get::<Option<String>, _>("thinking_efforts")
                    .expect("efforts stored"),
            )
            .expect("efforts are a JSON array");
            assert_eq!(
                efforts,
                ["none", "minimal", "low", "medium", "high", "xhigh", "max"],
                "{id} efforts",
            );
        }
    }
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p horsie-server migration_replaces_deepseek_chat 2>&1 | tail -20
```

Expected: FAIL — `no such column: base_url`.

- [ ] **Step 3: Write the migration**

Create `server/migrations/0013_deepseek_v4_and_card_base_url.sql`:

```sql
-- DeepSeek V4 support, plus a `base_url` on model cards.
--
-- `base_url` records where a model is officially served. It is reference data
-- only: nothing on the server reads it, and cards remain prefill templates that
-- are never linked to configured models (0008_model_cards.sql). Request routing
-- still reads providers.base_url alone.
--
-- `forced_tools_disable_thinking` marks backends that reject a pinned
-- tool_choice while thinking is enabled. DeepSeek returns 400 "Thinking mode
-- does not support this tool_choice" for tool_choice=required and for a named
-- function whenever thinking is on. Stored per model rather than inferred, for
-- the same reason thinking_dialect is (0011_thinking_efforts.sql).
ALTER TABLE model_cards ADD COLUMN base_url                      TEXT;
ALTER TABLE model_cards ADD COLUMN forced_tools_disable_thinking INTEGER NOT NULL DEFAULT 0;
ALTER TABLE models      ADD COLUMN forced_tools_disable_thinking INTEGER NOT NULL DEFAULT 0;

-- `deepseek-chat` is superseded, and its seeded limits (128k/8192) never
-- matched the V4 models. Deleting a card cannot affect a running deployment.
DELETE FROM model_cards WHERE model_id = 'deepseek-chat';

-- Seeding is insert-if-missing and so can never correct an existing row; the
-- V4 cards are therefore upserted here as well as added to the bundled seed.
-- Every value below is measured against the live API, not taken from the docs:
-- the published effort list (low/high/max) is stale — the API's own 400
-- enumerates all seven canonical values.
INSERT INTO model_cards (
    model_id, name, base_url, context_window, max_tokens,
    thinking_efforts, default_thinking_effort, thinking_dialect,
    forced_tools_disable_thinking
) VALUES
    ('deepseek-v4-flash', 'DeepSeek V4 Flash', 'https://api.deepseek.com', 1048576, 393216,
     '["none","minimal","low","medium","high","xhigh","max"]', 'high', 'openai_effort', 1),
    ('deepseek-v4-pro',   'DeepSeek V4 Pro',   'https://api.deepseek.com', 1048576, 393216,
     '["none","minimal","low","medium","high","xhigh","max"]', 'high', 'openai_effort', 1)
ON CONFLICT(model_id) DO UPDATE SET
    name                          = excluded.name,
    base_url                      = excluded.base_url,
    context_window                = excluded.context_window,
    max_tokens                    = excluded.max_tokens,
    thinking_efforts              = excluded.thinking_efforts,
    default_thinking_effort       = excluded.default_thinking_effort,
    thinking_dialect              = excluded.thinking_dialect,
    forced_tools_disable_thinking = excluded.forced_tools_disable_thinking,
    updated_at                    = datetime('now');
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
cargo test -p horsie-server migration_replaces_deepseek_chat 2>&1 | tail -20
```

Expected: PASS. If `sqlx::Row` is not already in scope for `.get(..)`, add
`use sqlx::Row;` inside `mod tests`.

- [ ] **Step 5: Confirm nothing else regressed**

```bash
cargo test -p horsie-server config:: 2>&1 | tail -20
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add server/migrations/0013_deepseek_v4_and_card_base_url.sql server/src/config/model_cards.rs
git commit -m "feat(server): add card base_url, forced-tools flag, DeepSeek V4 cards"
```

---

### Task 3: Model-card store carries the new columns

**Files:**
- Modify: `models/fluorite/model_cards.fl`
- Modify: `server/src/config/model_cards.rs` (`COLUMNS` ~55, `row_to_card` ~57, `insert` ~145, `update` ~176, `seed_if_missing` ~224, test helpers ~292)
- Regenerate: `clients/web/src/generated/model_cards/*.ts`

**Interfaces:**
- Consumes: the columns from Task 2.
- Produces: `ModelCard`/`ModelCardInput`/`ModelCardUpdate` fields `base_url: Option<String>` and `forced_tools_disable_thinking: Option<bool>`; TS `baseUrl?: string`, `forcedToolsDisableThinking?: boolean`. Tasks 5 and 6 consume these.

`forced_tools_disable_thinking` is `Option<bool>` on the wire so that omitting
it in JSON is legal (the seed file and existing API clients do exactly that);
it is stored as a non-null `INTEGER DEFAULT 0`, with `None` meaning false.

- [ ] **Step 1: Extend the schema**

In `models/fluorite/model_cards.fl`, add to **all three** structs (`ModelCard`,
`ModelCardInput`, `ModelCardUpdate`), after `thinking_dialect`:

```
    /// Where this model is officially served (e.g. "https://api.deepseek.com").
    /// Reference data only — nothing reads it, and an operator's configured
    /// provider base URL always wins.
    base_url: Option<String>,
    /// This backend rejects a pinned `tool_choice` while thinking is enabled,
    /// so thinking is disabled for those requests. Absent means false.
    forced_tools_disable_thinking: Option<bool>,
```

- [ ] **Step 2: Regenerate the TypeScript types**

```bash
cd clients/web && bun install && bun run generate-types && cd ../..
git diff --stat clients/web/src/generated
```

Expected: `modelCard.ts`, `modelCardInput.ts`, `modelCardUpdate.ts` each gain
`baseUrl?: string` and `forcedToolsDisableThinking?: boolean`.

If the `fluorite` CLI is not on PATH, install it with `cargo install fluorite`.
If it still cannot run, hand-edit those three generated files to match exactly
the shape above — they are committed to the repo.

- [ ] **Step 3: Write the failing test**

Add to `mod tests` in `server/src/config/model_cards.rs`:

```rust
    #[tokio::test]
    async fn base_url_and_forced_tools_flag_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;

        let mut card = input("ds", "DS", Some(1000), Some(100));
        card.base_url = Some("https://api.deepseek.com".into());
        card.forced_tools_disable_thinking = Some(true);
        let created = store.insert(&card).await.unwrap();
        assert_eq!(created.base_url.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(created.forced_tools_disable_thinking, Some(true));

        let fetched = store.get("ds").await.unwrap().unwrap();
        assert_eq!(fetched.base_url.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(fetched.forced_tools_disable_thinking, Some(true));

        let updated = store
            .update(
                "ds",
                &ModelCardUpdate {
                    name: "DS".into(),
                    context_window: Some(1000),
                    max_tokens: Some(100),
                    thinking_efforts: None,
                    default_thinking_effort: None,
                    thinking_dialect: None,
                    base_url: Some("https://proxy.example".into()),
                    forced_tools_disable_thinking: Some(false),
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.base_url.as_deref(), Some("https://proxy.example"));
        assert_eq!(updated.forced_tools_disable_thinking, Some(false));
    }

    /// Omitting the flag is legal and means false, so existing seed files and
    /// API clients keep working unchanged.
    #[tokio::test]
    async fn absent_flag_reads_back_as_false() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path()).await;
        store.insert(&input("plain", "Plain", None, None)).await.unwrap();
        let c = store.get("plain").await.unwrap().unwrap();
        assert_eq!(c.base_url, None);
        assert_eq!(c.forced_tools_disable_thinking, Some(false));
    }
```

- [ ] **Step 4: Run it to verify it fails**

```bash
cargo test -p horsie-server base_url_and_forced_tools 2>&1 | tail -20
```

Expected: compile error — `ModelCardInput` has no field `base_url`.

- [ ] **Step 5: Carry the columns through the store**

In `server/src/config/model_cards.rs`:

Extend `COLUMNS`:

```rust
const COLUMNS: &str = "model_id, name, context_window, max_tokens, thinking_efforts, default_thinking_effort, thinking_dialect, base_url, forced_tools_disable_thinking, created_at, updated_at";
```

In `row_to_card`, add before `created_at`:

```rust
        base_url: r.try_get("base_url")?,
        forced_tools_disable_thinking: Some(
            r.try_get::<i64, _>("forced_tools_disable_thinking")? != 0,
        ),
```

In `insert`, extend the column list and placeholders to include
`base_url, forced_tools_disable_thinking` / `?, ?`, and add the binds after
`thinking_dialect`:

```rust
        .bind(input.base_url.clone())
        .bind(i64::from(input.forced_tools_disable_thinking.unwrap_or(false)))
```

In `update`, add `base_url = ?, forced_tools_disable_thinking = ?,` to the SET
list (before `updated_at = datetime('now')`) and the matching binds **before**
the trailing `.bind(model_id)`:

```rust
        .bind(update.base_url.clone())
        .bind(i64::from(update.forced_tools_disable_thinking.unwrap_or(false)))
```

In `seed_if_missing`, extend the `INSERT OR IGNORE` column list and placeholders
the same way and add:

```rust
            .bind(c.base_url.clone())
            .bind(i64::from(c.forced_tools_disable_thinking.unwrap_or(false)))
```

Finally extend the test helper `fn input(..)` so the struct literal stays
exhaustive:

```rust
            base_url: None,
            forced_tools_disable_thinking: None,
```

- [ ] **Step 6: Fix the other `ModelCardUpdate` literals**

`crud_round_trip`, `update_and_delete_of_unknown_card_are_not_found` and
`seed_if_missing_never_overwrites_existing_rows` each build a `ModelCardUpdate`
and will no longer compile. Add to each literal:

```rust
                    base_url: None,
                    forced_tools_disable_thinking: None,
```

- [ ] **Step 7: Run the tests to verify they pass**

```bash
cargo fmt -p horsie-server
cargo test -p horsie-server config::model_cards 2>&1 | tail -20
```

Expected: PASS.

- [ ] **Step 8: Build the web client**

```bash
cd clients/web && bun run typecheck && cd ../..
```

Expected: no type errors.

- [ ] **Step 9: Commit**

```bash
git add models/fluorite/model_cards.fl server/src/config/model_cards.rs clients/web/src/generated
git commit -m "feat(server): store base_url and forced-tools flag on model cards"
```

---

### Task 4: Wire the flag from the `models` table into `OpenAiProvider`

**Files:**
- Modify: `models/fluorite/settings.fl` (`ModelView` ~23, `ModelInput` ~109)
- Modify: `server/src/config/store.rs` (`ModelRow` ~296, `default_context_window` ~307, `build_registry` ~326, `build_openai` ~399, `model_view` ~464, `read_models` ~525, model INSERT ~225)
- Regenerate: `clients/web/src/generated/settings/*.ts`

**Interfaces:**
- Consumes: `OpenAiProvider::with_forced_tools_disable_thinking` (Task 1); `models.forced_tools_disable_thinking` (Task 2).
- Produces: `ModelInput`/`ModelView` field `forced_tools_disable_thinking: Option<bool>` (TS `forcedToolsDisableThinking?: boolean`), consumed by Task 6.

- [ ] **Step 1: Extend the schema**

In `models/fluorite/settings.fl`, add to **both** `ModelView` and `ModelInput`,
after `thinking_dialect`:

```
    /// This backend rejects a pinned `tool_choice` while thinking is enabled,
    /// so thinking is disabled for those requests. Absent means false.
    forced_tools_disable_thinking: Option<bool>,
```

- [ ] **Step 2: Regenerate the TypeScript types**

```bash
cd clients/web && bun run generate-types && cd ../..
```

- [ ] **Step 3: Write the failing test**

Add to `mod tests` in `server/src/config/store.rs`:

```rust
    /// The flag has to survive the save→read→build round trip, because it is
    /// what keeps a forced-handoff agent from 400ing on DeepSeek.
    #[tokio::test]
    async fn forced_tools_flag_persists_through_a_settings_update() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path()).await.store;

        store
            .update(SettingsUpdate {
                providers: Some(vec![ProviderInput {
                    name: "deepseek".into(),
                    kind: "openai".into(),
                    base_url: Some("https://api.deepseek.com".into()),
                    api_key: Some("k".into()),
                    keep_thinking_signature: None,
                }]),
                models: Some(vec![ModelInput {
                    alias: "ds".into(),
                    provider: "deepseek".into(),
                    model_id: "deepseek-v4-flash".into(),
                    max_tokens: Some(393_216),
                    context_window: None,
                    thinking_efforts: Some(vec!["none".into(), "high".into()]),
                    thinking_effort: Some("high".into()),
                    thinking_dialect: Some("openai_effort".into()),
                    forced_tools_disable_thinking: Some(true),
                }]),
                default_vendor: None,
            })
            .await
            .expect("update succeeds");

        let view = store.view().await.expect("view");
        let m = view.models.iter().find(|m| m.alias == "ds").expect("model");
        assert_eq!(m.forced_tools_disable_thinking, Some(true));
        // The built-in default for a "deepseek" model id is the real window.
        assert_eq!(m.context_window, Some(1_048_576));
    }
```

`open(dir)` is the existing test helper in this module; it returns an
`OpenedConfig`, whose `.store` field is the `Arc<DbConfigStore>`.

- [ ] **Step 4: Run it to verify it fails**

```bash
cargo test -p horsie-server forced_tools_flag_persists 2>&1 | tail -20
```

Expected: compile error — `ModelInput` has no field `forced_tools_disable_thinking`.

- [ ] **Step 5: Carry it through the store**

In `server/src/config/store.rs`:

Add to `struct ModelRow`:

```rust
    forced_tools_disable_thinking: bool,
```

In the models `INSERT` inside `update`, add the column and placeholder and bind
after `thinking_dialect`:

```rust
                .bind(i64::from(m.forced_tools_disable_thinking.unwrap_or(false)))
```

In `read_models`, add `forced_tools_disable_thinking` to the `SELECT` list and:

```rust
            forced_tools_disable_thinking: r
                .try_get::<i64, _>("forced_tools_disable_thinking")?
                != 0,
```

In `model_view`, add:

```rust
        forced_tools_disable_thinking: Some(r.forced_tools_disable_thinking),
```

In `build_registry`, pass it to the `"openai"` arm only:

```rust
            "openai" => build_openai(
                p.base_url.as_deref(),
                p.api_key.as_deref(),
                &m.model_id,
                max_tokens,
                dialect,
                m.forced_tools_disable_thinking,
            )?,
```

Leave the `"anthropic"` arm untouched — the Anthropic wire has no such conflict.
Add that as a comment above the match arm:

```rust
        // Only the OpenAI wire takes the forced-tools flag: Anthropic accepts a
        // pinned tool_choice with thinking enabled, so there is nothing to
        // reconcile there.
```

In `build_openai`, take the new parameter and apply it:

```rust
fn build_openai(
    base_url: Option<&str>,
    api_key: Option<&str>,
    model_id: &str,
    max_tokens: Option<u32>,
    thinking_dialect: ThinkingDialect,
    forced_tools_disable_thinking: bool,
) -> Result<Arc<dyn LlmProvider>, String> {
```

and extend the builder chain:

```rust
    p = p
        .with_model(model_id)
        .with_max_tokens(max_tokens)
        .with_thinking_dialect(thinking_dialect)
        .with_forced_tools_disable_thinking(forced_tools_disable_thinking);
```

- [ ] **Step 6: Correct the built-in context window**

In `default_context_window`, replace the `deepseek` entry:

```rust
        ("deepseek", 1_048_576),
```

- [ ] **Step 7: Run the tests to verify they pass**

```bash
cargo fmt -p horsie-server
cargo test -p horsie-server config:: 2>&1 | tail -20
```

Expected: PASS. Other tests in this module that build a `ModelInput` literal
need `forced_tools_disable_thinking: None` added; fix each compile error the
same way.

- [ ] **Step 8: Commit**

```bash
git add models/fluorite/settings.fl server/src/config/store.rs clients/web/src/generated
git commit -m "feat(server): route the forced-tools flag into the OpenAI provider"
```

---

### Task 5: Bundled seed catalog

**Files:**
- Modify: `server/src/config/model_cards_seed.json` (last entry)
- Test: `server/src/config/model_cards.rs`

**Interfaces:**
- Consumes: `ModelCardInput` fields from Task 3.
- Produces: nothing new.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `server/src/config/model_cards.rs`:

```rust
    #[tokio::test]
    async fn bundled_seed_carries_the_deepseek_v4_cards() {
        let cards = bundled_seed().expect("bundled seed parses");
        assert!(
            !cards.iter().any(|c| c.model_id == "deepseek-chat"),
            "deepseek-chat is superseded and must not be seeded",
        );
        for id in ["deepseek-v4-flash", "deepseek-v4-pro"] {
            let c = cards
                .iter()
                .find(|c| c.model_id == id)
                .unwrap_or_else(|| panic!("catalog includes {id}"));
            assert_eq!(c.base_url.as_deref(), Some("https://api.deepseek.com"));
            assert_eq!(c.context_window, Some(1_048_576));
            assert_eq!(c.max_tokens, Some(393_216));
            assert_eq!(c.thinking_dialect.as_deref(), Some("openai_effort"));
            assert_eq!(c.default_thinking_effort.as_deref(), Some("high"));
            assert_eq!(c.forced_tools_disable_thinking, Some(true));
            assert_eq!(
                c.thinking_efforts.as_ref().expect("efforts listed").as_slice(),
                ["none", "minimal", "low", "medium", "high", "xhigh", "max"],
            );
        }
    }
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p horsie-server bundled_seed_carries_the_deepseek 2>&1 | tail -20
```

Expected: FAIL — `catalog includes deepseek-v4-flash`.

- [ ] **Step 3: Replace the seed entry**

In `server/src/config/model_cards_seed.json`, replace the final line

```json
  { "modelId": "deepseek-chat", "name": "DeepSeek Chat", "contextWindow": 128000, "maxTokens": 8192, "thinkingDialect": "none" }
```

with

```json
  { "modelId": "deepseek-v4-flash", "name": "DeepSeek V4 Flash", "baseUrl": "https://api.deepseek.com", "contextWindow": 1048576, "maxTokens": 393216, "thinkingEfforts": ["none","minimal","low","medium","high","xhigh","max"], "defaultThinkingEffort": "high", "thinkingDialect": "openai_effort", "forcedToolsDisableThinking": true },
  { "modelId": "deepseek-v4-pro", "name": "DeepSeek V4 Pro", "baseUrl": "https://api.deepseek.com", "contextWindow": 1048576, "maxTokens": 393216, "thinkingEfforts": ["none","minimal","low","medium","high","xhigh","max"], "defaultThinkingEffort": "high", "thinkingDialect": "openai_effort", "forcedToolsDisableThinking": true }
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test -p horsie-server config::model_cards 2>&1 | tail -20
```

Expected: PASS. In particular `bundled_seed_efforts_and_dialects_are_canonical`
must still pass — `openai_effort` supports every canonical effort, so all seven
are legal.

- [ ] **Step 5: Commit**

```bash
git add server/src/config/model_cards_seed.json server/src/config/model_cards.rs
git commit -m "feat(server): seed the DeepSeek V4 model cards"
```

---

### Task 6: Web UI

**Files:**
- Modify: `clients/web/src/pages/admin/ModelCardsPage.tsx`
- Modify: `clients/web/src/pages/settings/ModelsSettings.tsx`
- Test: `clients/web/e2e/k-model-cards.spec.ts`

**Interfaces:**
- Consumes: TS types from Tasks 3 and 4.
- Produces: nothing.

Note the deliberate asymmetry: the admin card page is the **only** surface that
reads or writes `base_url`. `ModelsSettings.pick(card)` must keep ignoring it —
prefilling a provider's base URL from a card is explicitly out of scope.

- [ ] **Step 1: Add the card fields to the admin page**

In `ModelCardsPage.tsx`, add state next to `maxTokens`:

```tsx
  const [baseUrl, setBaseUrl] = useState(card?.baseUrl ?? "");
  const [forcedToolsDisableThinking, setForcedToolsDisableThinking] = useState(
    card?.forcedToolsDisableThinking ?? false,
  );
```

Add two controls inside the existing `grid grid-cols-2 gap-3`, after the max
tokens field:

```tsx
        <label className="block">
          <RowLabel>Base URL (optional)</RowLabel>
          <input
            className="input font-mono"
            value={baseUrl}
            onChange={(e) => {
              setBaseUrl(e.target.value);
              touch();
            }}
            placeholder="https://api.deepseek.com"
            data-testid="model-card-base-url"
          />
        </label>
        <label className="col-span-2 flex items-start gap-2 text-sm">
          <input
            type="checkbox"
            className="mt-1"
            checked={forcedToolsDisableThinking}
            onChange={(e) => {
              setForcedToolsDisableThinking(e.target.checked);
              touch();
            }}
            data-testid="model-card-forced-tools"
          />
          <span>
            Pinned tool choice disables thinking
            <span className="block text-xs opacity-70">
              For backends that reject a forced <code>tool_choice</code> while thinking is on —
              DeepSeek answers 400 “Thinking mode does not support this tool_choice”.
            </span>
          </span>
        </label>
```

Include both in the create and update payloads:

```tsx
          baseUrl: baseUrl.trim() || undefined,
          forcedToolsDisableThinking,
```

- [ ] **Step 2: Add the model flag to the settings form**

In `ModelsSettings.tsx`:

Add to `type ModelDraft`:

```tsx
  forcedToolsDisableThinking: boolean;
```

Add to `toModelDrafts`:

```tsx
    forcedToolsDisableThinking: m.forcedToolsDisableThinking ?? false,
```

Add to the new-model literal in the "Models" `onAdd`:

```tsx
                      forcedToolsDisableThinking: false,
```

Add to `modelInputs` in `save`:

```tsx
      forcedToolsDisableThinking: m.forcedToolsDisableThinking,
```

Add to `pick(card)` in `ModelIdField`, following the existing
"only when still empty" prefill contract — here the empty state is `false`:

```tsx
      forcedToolsDisableThinking:
        draft.forcedToolsDisableThinking || (card.forcedToolsDisableThinking ?? false),
```

Add the control to `ModelRow`, inside the `col-span-2 border-t pt-3` block after
the Default effort / Wire dialect grid:

```tsx
          <label className="mt-2 flex items-start gap-2 text-sm">
            <input
              type="checkbox"
              className="mt-1"
              checked={draft.forcedToolsDisableThinking}
              onChange={(ev) => set({ forcedToolsDisableThinking: ev.target.checked })}
              data-testid="model-forced-tools"
            />
            <span>
              Pinned tool choice disables thinking
              <span className="block text-xs opacity-70">
                Required for DeepSeek, which rejects a forced tool choice while thinking is on.
                Sub-agents that must call a handoff tool will run without thinking.
              </span>
            </span>
          </label>
```

- [ ] **Step 3: Typecheck**

```bash
cd clients/web && bun run typecheck && cd ../..
```

Expected: no errors.

- [ ] **Step 4: Write the e2e assertions**

Add inside the existing `test.describe("model cards", ...)` block in
`clients/web/e2e/k-model-cards.spec.ts`. The file imports
`{ test, expect } from "./fixtures"` and its tests take `{ page, appBase }` and
navigate with `page.goto(\`${appBase}/...\`)` — follow that, and note the
suggestion list only appears while the model-id input is focused:

```ts
  test("a card's flag prefills the model row while its base URL leaves providers alone", async ({
    page,
    appBase,
  }) => {
    // The seeded deepseek-v4-flash card carries both forcedToolsDisableThinking
    // and a baseUrl. Picking it must copy the flag onto the model draft and
    // leave every provider's base URL untouched — prefilling that is out of
    // scope for this change.
    await page.goto(`${appBase}/settings/models`);

    const providerBaseUrl = page.getByLabel("Base URL (optional)").first();
    const before = await providerBaseUrl.inputValue();

    const modelId = page.getByTestId("model-id-input").last();
    await modelId.click();
    await modelId.fill("deepseek-v4-flash");
    await page
      .getByTestId("model-card-suggestion-deepseek-v4-flash")
      .click();

    await expect(page.getByTestId("model-forced-tools").last()).toBeChecked();
    await expect(providerBaseUrl).toHaveValue(before);
  });
```

- [ ] **Step 5: Run the e2e suite**

```bash
cd clients/web && bun run test:e2e k-model-cards && cd ../..
```

Expected: PASS. `test:e2e` maps to `playwright test`; the harness starts a real
server, so the seeded catalog really does contain the DeepSeek cards.

- [ ] **Step 6: Commit**

```bash
git add clients/web/src/pages/admin/ModelCardsPage.tsx clients/web/src/pages/settings/ModelsSettings.tsx clients/web/e2e/k-model-cards.spec.ts
git commit -m "feat(web): edit card base URL and the pinned-tool thinking flag"
```

---

### Task 7: Docs and a live smoke test

**Files:**
- Modify: `docs/guide/settings-reference.md` (the only guide page covering provider kinds)
- Create: `providers/openai/tests/deepseek_live.rs`

**Interfaces:**
- Consumes: `OpenAiProvider::with_forced_tools_disable_thinking` (Task 1).
- Produces: nothing.

- [ ] **Step 1: Write the live smoke test**

Create `providers/openai/tests/deepseek_live.rs`:

```rust
//! Live DeepSeek checks. Ignored by default — they cost money and need a key.
//!
//! Run with:
//!   DEEPSEEK_API_KEY=sk-... cargo test -p horsie-openai --test deepseek_live -- --ignored

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use horsie_agentcore::{
    CompletionRequest, EventSink, EventSinkError, LlmProvider, ThinkingDialect, ThinkingEffort,
    ToolChoice, ToolSpec,
};
use horsie_models::agent::{ContentPart, Message, Role, TextPart};
use horsie_models::events::AgentEvent;
use horsie_openai::OpenAiProvider;

/// Defined here rather than reaching for `agentcore::testkit`, which is behind
/// the `test-util` feature that this crate's dev-dependencies do not enable.
struct NullSink;

#[async_trait]
impl EventSink for NullSink {
    async fn emit(&self, _event: AgentEvent) -> Result<(), EventSinkError> {
        Ok(())
    }
}

fn key() -> String {
    std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY must be set for live tests")
}

/// The regression this whole feature exists for: DeepSeek 400s on a pinned
/// tool_choice while thinking is on, so the flag must make it succeed.
#[tokio::test]
#[ignore = "hits the live DeepSeek API"]
async fn pinned_tool_choice_succeeds_with_the_flag() {
    let provider = OpenAiProvider::with_api_key(key().as_str())
        .unwrap()
        .with_model("deepseek-v4-flash")
        .with_base_url("https://api.deepseek.com")
        .with_thinking_dialect(ThinkingDialect::OpenAiEffort)
        .with_forced_tools_disable_thinking(true);

    let messages = vec![Message {
        id: "m1".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "What is the weather in Paris?".into(),
        })],
    }];
    let request = CompletionRequest {
        messages: &messages,
        system: None,
        tools: vec![ToolSpec {
            name: "get_weather".into(),
            description: "Get the current weather for a city.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
            }),
        }],
        tool_choice: ToolChoice::Any,
        max_tokens: Some(512),
        thinking_effort: ThinkingEffort::parse("high"),
    };

    // `complete` takes the request by value, plus the assistant message id.
    let resp = provider
        .complete(request, "msg-1", &NullSink)
        .await
        .expect("a pinned tool choice must not 400");

    assert!(
        resp.parts
            .iter()
            .any(|p| matches!(p, ContentPart::ToolCall(_))),
        "expected a tool call",
    );
}
```

The variant is `ContentPart::ToolCall`, not `ToolUse` — `ToolUse` is a
`StopReason`. No `Cargo.toml` change is needed: `async-trait`, `serde_json`,
`horsie-models` and `horsie-agentcore` are already normal dependencies of this
crate, and the dev `tokio` already has `macros` and `rt-multi-thread`.

- [ ] **Step 2: Verify it compiles and is skipped by default**

```bash
cargo test -p horsie-openai --test deepseek_live 2>&1 | tail -20
```

Expected: compiles; `0 passed; 0 failed; 1 ignored`.

- [ ] **Step 3: Document adding DeepSeek**

Add a subsection to the provider/model guide page:

```markdown
### DeepSeek

DeepSeek speaks the OpenAI wire, so it needs no special provider kind:

- **Kind:** `OpenAI-compatible`
- **Base URL:** `https://api.deepseek.com`
- **Models:** `deepseek-v4-flash`, `deepseek-v4-pro`

Both models are in the bundled card catalog, so picking the model id in
Settings → Models fills in the context window (1,048,576), the generation cap
(393,216) and the thinking configuration for you.

Thinking is on by default and accepts the full effort ladder — `none`, `minimal`,
`low`, `medium`, `high`, `xhigh`, `max` — despite DeepSeek's own documentation
listing only three of them.

One constraint is worth knowing before you pick DeepSeek for sub-agents.
DeepSeek rejects a pinned tool choice while thinking is enabled, answering
`400 Thinking mode does not support this tool_choice`. The model card's
**Pinned tool choice disables thinking** flag handles this by turning thinking
off for exactly those requests. Since a forced-handoff agent pins a tool on
*every* turn, such an agent runs with thinking off throughout — so DeepSeek is a
weak choice for handoff-style sub-agents, though ordinary sessions are
unaffected.
```

- [ ] **Step 4: Commit**

```bash
git add docs/guide providers/openai/tests/deepseek_live.rs
git commit -m "docs: how to add DeepSeek, and a live pinned-tool smoke test"
```

---

### Task 8: Full verification

**Files:** none.

- [ ] **Step 1: Full test suite**

```bash
cargo fmt --all
cargo test --workspace 2>&1 | tail -30
```

Expected: all pass.

- [ ] **Step 2: Lint**

```bash
cargo clippy --workspace --all-targets 2>&1 | tail -30
```

Expected: no warnings.

- [ ] **Step 3: Web build**

```bash
cd clients/web && bun run typecheck && bun run build && cd ../..
```

Expected: clean.

- [ ] **Step 4: Live verification against DeepSeek**

```bash
DEEPSEEK_API_KEY=<key> cargo test -p horsie-openai --test deepseek_live -- --ignored --nocapture 2>&1 | tail -20
```

Expected: PASS. This is the one check that proves the feature works against the
real API rather than against our understanding of it.

- [ ] **Step 5: Push and open the PR**

Write the body as one long line per paragraph — GitHub renders newlines as
literal breaks, so never hard-wrap it.

```bash
git push -u origin feat/deepseek-models
gh pr create --title "DeepSeek V4 model support" --body "$(cat <<'EOF'
Adds `deepseek-v4-flash` and `deepseek-v4-pro` as first-class models. DeepSeek rides the existing `openai` provider kind — no new kind and no new thinking dialect, because probing the live API showed `openai_effort` is already the correct encoding and cache tokens are already read correctly.

The one real blocker was forced tool choice: DeepSeek returns `400 Thinking mode does not support this tool_choice` for `tool_choice: "required"` and for a named function whenever thinking is on, and `agentcore` pins a tool on every turn of a forced-handoff agent. A new per-model `forced_tools_disable_thinking` flag, carried on the model card and the `models` row, makes `OpenAiProvider` send `reasoning_effort: "none"` on exactly those requests. It sits outside the dialect match because a model with no thinking control still thinks by default on such a backend.

Model cards also gain a `base_url` column recording where a model is officially served. It is stored and admin-editable; nothing reads it yet.

Two documented facts turned out to be wrong and the code follows the API instead: effort is only honored at the top level (the nested `thinking.reasoning_effort` is silently ignored), and all seven canonical effort values are accepted, not the three the docs list. Limits are measured: 1,048,576 context, `max_tokens` capped at 393,216.

Verified against the live API, including the pinned-tool case, via an `#[ignore]`d smoke test gated on `DEEPSEEK_API_KEY`.
EOF
)"
```

---

## Notes for the implementer

**A pre-existing bug you will notice but must not fix here.** The admin model
card form (`ModelCardsPage.tsx`) does not expose `thinkingEfforts`,
`defaultThinkingEffort` or `thinkingDialect`, yet its update payload omits them
— and `ModelCardStore::update` writes `NULL` for every field it is given. So
saving any card from the admin UI silently wipes its thinking metadata. Task 6
adds two more fields to that form and keeps them consistent, but does not fix
the wipe. Raise it separately; folding it in would make this PR's diff span an
unrelated defect.
