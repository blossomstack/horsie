# Per-Model Thinking Efforts — Server Implementation Plan (Plan C)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a session choose a thinking effort from the menu its model offers, and translate that choice into the right wire fields for that model.

**Architecture:** A canonical effort vocabulary (`none|minimal|low|medium|high|xhigh|max`) plus a per-model *dialect* naming the wire encoding. The effort value rides per-call on `CompletionRequest` (sessions differ); the dialect rides on the provider instance (models differ, and the registry is keyed by model alias). Each provider adapter owns its own translation.

**Tech Stack:** Rust (axum, sqlx/SQLite), fluorite-codegen wire models.

## Global Constraints

- Depends on **Plan B** — `async_llm::types::OutputConfig` must exist and horsie must be pinned at async-llm `main`. Do not start Task 3 before Plan B Task 3 is merged.
- Worktree `.horsie/worktrees/thinking-config`, branch `feat/thinking-efforts` (Plan A's three commits are already there).
- Canonical values are `String` end-to-end, never fluorite enums — the `providers.kind` precedent; PascalCase-ing a live string column breaks the DB.
- New wire fields on **input** types must be `Option<...>`. Plan A proved this: a required `bool` on `ProviderInput` broke `PUT /api/config` with a 422 (`config_get_and_put_round_trip`).
- New columns are `ALTER TABLE ... ADD COLUMN` with defaults; NULL means "no thinking control", i.e. no behaviour change for existing rows.
- New fields on the storage twin `AgentSettings` need `#[serde(default)]` so pre-existing journal rows still deserialize — the pattern `mcp_servers` and `memory_spaces` already use.
- Editing any `.fl` requires regenerating the TS: `cd clients/web && npm run generate-types`. The generated files are checked in.
- Test modules that use `panic!`/`unwrap`/wildcard match arms need the crate's allow block:
  `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]`.
- Never list Claude as author/co-author on commits.

---

### Task 1: Canonical effort vocabulary and dialects

Pure logic, no I/O — the piece every later task depends on.

**Files:**
- Create: `agentcore/src/thinking.rs`
- Modify: `agentcore/src/lib.rs` (declare + re-export)

**Interfaces:**
- Produces: `ThinkingEffort` (`parse`, `as_str`, `is_none`), `ThinkingDialect` (`parse`, `as_str`, `supports`), both `String`-backed.

- [ ] **Step 1: Write the failing test**

Create `agentcore/src/thinking.rs` with only the tests:

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_canonical_value() {
        for v in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            assert_eq!(ThinkingEffort::parse(v).expect("canonical").as_str(), v);
        }
    }

    #[test]
    fn rejects_unknown_effort() {
        assert!(ThinkingEffort::parse("ultra").is_none());
        assert!(ThinkingEffort::parse("").is_none());
    }

    #[test]
    fn none_is_distinguishable() {
        assert!(ThinkingEffort::parse("none").expect("canonical").is_none_effort());
        assert!(!ThinkingEffort::parse("low").expect("canonical").is_none_effort());
    }

    #[test]
    fn parses_every_dialect() {
        for d in [
            "anthropic_effort",
            "anthropic_always_on",
            "anthropic_budget",
            "openai_effort",
            "zai_thinking",
            "kimi_thinking",
            "none",
        ] {
            assert_eq!(ThinkingDialect::parse(d).expect("known dialect").as_str(), d);
        }
        assert!(ThinkingDialect::parse("bogus").is_none());
    }

    #[test]
    fn always_on_dialect_rejects_none_effort() {
        let d = ThinkingDialect::parse("anthropic_always_on").expect("known");
        let none = ThinkingEffort::parse("none").expect("canonical");
        let high = ThinkingEffort::parse("high").expect("canonical");
        assert!(!d.supports(&none), "Fable 5 cannot disable thinking");
        assert!(d.supports(&high));
    }

    #[test]
    fn none_dialect_supports_nothing() {
        let d = ThinkingDialect::parse("none").expect("known");
        for v in ["none", "low", "high", "max"] {
            assert!(!d.supports(&ThinkingEffort::parse(v).expect("canonical")));
        }
    }

    #[test]
    fn effort_dialects_support_all_values() {
        for name in ["anthropic_effort", "openai_effort"] {
            let d = ThinkingDialect::parse(name).expect("known");
            for v in ["none", "low", "medium", "high", "xhigh", "max"] {
                assert!(d.supports(&ThinkingEffort::parse(v).expect("canonical")));
            }
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p horsie-agentcore thinking`
Expected: FAIL to compile — `cannot find type 'ThinkingEffort'`.

- [ ] **Step 3: Write the implementation**

Prepend to `agentcore/src/thinking.rs`:

```rust
//! Canonical thinking-effort vocabulary and the per-model wire dialects that
//! encode it.
//!
//! The *value* is portable across vendors; the *encoding* is not. Two models on
//! the same provider kind can need different request shapes (Anthropic Opus 4.8
//! takes `output_config.effort`, Haiku 4.5 rejects `effort` entirely), and one
//! model can need a different encoding than its provider kind implies (Kimi k3
//! on the Anthropic wire silently ignores `reasoning_effort`). So the dialect is
//! stored per model rather than inferred from the provider.

/// A canonical effort value. Absence of a value means "send no thinking
/// control at all"; [`ThinkingEffort::None`] means "explicitly disable".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ThinkingEffort {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Self::None,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::XHigh,
            "max" => Self::Max,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// True for the explicit "disable thinking" value.
    pub fn is_none_effort(self) -> bool {
        matches!(self, Self::None)
    }
}

/// How a canonical effort is encoded on the wire for a given model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingDialect {
    /// `output_config.effort` + `thinking:{type:"adaptive"}`; `none` becomes
    /// `thinking:{type:"disabled"}`.
    AnthropicEffort,
    /// `output_config.effort` only, `thinking` omitted. Thinking cannot be
    /// disabled (Fable 5 rejects `{type:"disabled"}` with a 400).
    AnthropicAlwaysOn,
    /// `thinking:{type:"enabled",budget_tokens:N}` / `{type:"disabled"}`;
    /// `output_config.effort` only when the model offers effort values.
    AnthropicBudget,
    /// Top-level `reasoning_effort`.
    OpenAiEffort,
    /// `thinking:{type:"enabled"|"disabled"}` — toggle only, no effort.
    ZaiThinking,
    /// `thinking:{type:"enabled",keep:"all"}` — the only legal value.
    KimiThinking,
    /// No thinking control at all.
    NoControl,
}

impl ThinkingDialect {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "anthropic_effort" => Self::AnthropicEffort,
            "anthropic_always_on" => Self::AnthropicAlwaysOn,
            "anthropic_budget" => Self::AnthropicBudget,
            "openai_effort" => Self::OpenAiEffort,
            "zai_thinking" => Self::ZaiThinking,
            "kimi_thinking" => Self::KimiThinking,
            "none" => Self::NoControl,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicEffort => "anthropic_effort",
            Self::AnthropicAlwaysOn => "anthropic_always_on",
            Self::AnthropicBudget => "anthropic_budget",
            Self::OpenAiEffort => "openai_effort",
            Self::ZaiThinking => "zai_thinking",
            Self::KimiThinking => "kimi_thinking",
            Self::NoControl => "none",
        }
    }

    /// Whether this dialect can express the given effort at all. Config-time
    /// validation uses this so an impossible combination is rejected before it
    /// reaches a provider.
    pub fn supports(self, effort: ThinkingEffort) -> bool {
        match self {
            Self::AnthropicEffort | Self::OpenAiEffort => true,
            Self::AnthropicAlwaysOn => !effort.is_none_effort(),
            Self::AnthropicBudget => true,
            Self::ZaiThinking | Self::KimiThinking => effort.is_none_effort(),
            Self::NoControl => false,
        }
    }
}
```

Note the tests call `supports(&effort)` but the method takes `ThinkingEffort` by value (it is `Copy`). Change the test call sites to `supports(effort)` when you paste them in, or the compiler will tell you.

- [ ] **Step 4: Declare the module**

In `agentcore/src/lib.rs`, add `pub mod thinking;` alongside the existing module declarations, and re-export the two types next to the other `pub use` items:

```rust
pub use thinking::{ThinkingDialect, ThinkingEffort};
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p horsie-agentcore thinking`
Expected: PASS, 7 tests.

- [ ] **Step 6: Commit**

```bash
git add agentcore/src/thinking.rs agentcore/src/lib.rs
git commit -m "feat(agentcore): canonical thinking effort vocabulary and dialects"
```

---

### Task 2: Schema and config plumbing

**Files:**
- Create: `server/migrations/0011_thinking_efforts.sql`
- Modify: `models/fluorite/model_cards.fl`, `models/fluorite/settings.fl`
- Modify: `server/src/config/store.rs` (`ModelRow`, model INSERT/SELECT, `model_view`)
- Modify: `server/src/config/model_cards.rs` (`COLUMNS` and the row mapping)

**Interfaces:**
- Consumes: `ThinkingEffort`, `ThinkingDialect` (Task 1).
- Produces: `ModelRow.thinking_efforts: Option<String>` (JSON array), `.thinking_effort: Option<String>`, `.thinking_dialect: Option<String>`; same three on `ModelView`, and as `Option<...>` on `ModelInput`.

- [ ] **Step 1: Write the migration**

Create `server/migrations/0011_thinking_efforts.sql`:

```sql
-- Per-model thinking configuration.
--
-- `thinking_efforts` is a JSON array of canonical effort values
-- ("none","minimal","low","medium","high","xhigh","max") that this model
-- offers; a session picks one. `thinking_effort` is the default applied when a
-- session does not choose. `thinking_dialect` names the wire encoding — two
-- models on the same provider kind can need different shapes, so this is data,
-- not inference. NULL everywhere means "send no thinking control", preserving
-- existing behaviour.
--
-- Cards are reference data (what the provider supports); the `models` copy is
-- the deployment's editable menu, prefilled from the card.
ALTER TABLE model_cards ADD COLUMN thinking_efforts        TEXT;
ALTER TABLE model_cards ADD COLUMN default_thinking_effort TEXT;
ALTER TABLE model_cards ADD COLUMN thinking_dialect        TEXT;

ALTER TABLE models ADD COLUMN thinking_efforts TEXT;
ALTER TABLE models ADD COLUMN thinking_effort  TEXT;
ALTER TABLE models ADD COLUMN thinking_dialect TEXT;
```

- [ ] **Step 2: Write the failing round-trip test**

Append to `mod tests` in `server/src/config/store.rs`:

```rust
    #[tokio::test]
    async fn model_thinking_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;

        let mut m = model("m", "p");
        m.thinking_efforts = Some(vec!["none".into(), "low".into(), "high".into()]);
        m.thinking_effort = Some("high".into());
        m.thinking_dialect = Some("anthropic_effort".into());

        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-test"))]),
                models: Some(vec![m]),
                vendors: None,
                default_vendor: None,
            })
            .await
            .expect("update succeeds");

        let got = &view.models[0];
        assert_eq!(
            got.thinking_efforts.as_deref(),
            Some(&["none".to_string(), "low".to_string(), "high".to_string()][..])
        );
        assert_eq!(got.thinking_effort.as_deref(), Some("high"));
        assert_eq!(got.thinking_dialect.as_deref(), Some("anthropic_effort"));
    }

    #[tokio::test]
    async fn model_thinking_config_defaults_to_absent() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let view = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-test"))]),
                models: Some(vec![model("m", "p")]),
                vendors: None,
                default_vendor: None,
            })
            .await
            .expect("update succeeds");
        assert_eq!(view.models[0].thinking_efforts, None);
        assert_eq!(view.models[0].thinking_effort, None);
        assert_eq!(view.models[0].thinking_dialect, None);
    }

    #[tokio::test]
    async fn model_rejects_effort_outside_its_menu() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let mut m = model("m", "p");
        m.thinking_efforts = Some(vec!["low".into()]);
        m.thinking_effort = Some("max".into());
        m.thinking_dialect = Some("anthropic_effort".into());
        let err = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-test"))]),
                models: Some(vec![m]),
                vendors: None,
                default_vendor: None,
            })
            .await
            .expect_err("default effort must be one the model offers");
        assert!(err.contains("max"), "error should name the bad value: {err}");
    }

    #[tokio::test]
    async fn model_rejects_unknown_dialect() {
        let dir = tempfile::tempdir().unwrap();
        let o = open(dir.path()).await;
        let mut m = model("m", "p");
        m.thinking_dialect = Some("telepathy".into());
        let err = o
            .store
            .update(SettingsUpdate {
                providers: Some(vec![provider("p", Some("sk-test"))]),
                models: Some(vec![m]),
                vendors: None,
                default_vendor: None,
            })
            .await
            .expect_err("unknown dialect must be rejected");
        assert!(err.contains("telepathy"), "error should name it: {err}");
    }
```

Extend the `model(alias, provider)` test helper with the three new fields set to `None`.

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p horsie-server model_thinking`
Expected: FAIL to compile — `ModelInput` has no field `thinking_efforts`.

- [ ] **Step 4: Add the wire fields**

In `models/fluorite/settings.fl`, add to `ModelView`:

```
    /// Canonical thinking-effort values this model offers, in ascending order.
    /// Absent → the model exposes no thinking control.
    thinking_efforts: Option<Vec<String>>,
    /// Default applied when a session does not choose one.
    thinking_effort: Option<String>,
    /// Wire encoding for this model's thinking control.
    thinking_dialect: Option<String>,
```

Add the same three to `ModelInput` (all already `Option`, so no compat break).

In `models/fluorite/model_cards.fl`, add to `ModelCard`, `ModelCardInput`, and `ModelCardUpdate`:

```
    thinking_efforts: Option<Vec<String>>,
    default_thinking_effort: Option<String>,
    thinking_dialect: Option<String>,
```

- [ ] **Step 5: Regenerate TS types**

```bash
cd clients/web && npm run generate-types && cd ../..
```

- [ ] **Step 6: Thread through the store**

`ModelRow` gains the three fields as `Option<String>` (the efforts list stays JSON-encoded at rest — the `vendors.config` precedent).

Add a helper near `model_view` in `server/src/config/store.rs`:

```rust
/// Efforts are stored as a JSON array in a TEXT column; a malformed value is
/// treated as absent rather than failing the whole settings read.
fn decode_efforts(raw: Option<&str>) -> Option<Vec<String>> {
    raw.and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
}

fn encode_efforts(list: Option<&Vec<String>>) -> Option<String> {
    list.and_then(|v| serde_json::to_string(v).ok())
}
```

Extend the model INSERT and SELECT with the three columns, mapping through those helpers, and populate `model_view` from them.

- [ ] **Step 7: Validate on write**

In the model loop of `update()` (next to the existing provider-kind check), add:

```rust
                if let Some(d) = m.thinking_dialect.as_deref()
                    && horsie_agentcore::ThinkingDialect::parse(d).is_none()
                {
                    return Err(format!("model '{}' has unknown thinking dialect '{d}'", m.alias));
                }
                let offered: Vec<String> = m.thinking_efforts.clone().unwrap_or_default();
                for e in &offered {
                    if horsie_agentcore::ThinkingEffort::parse(e).is_none() {
                        return Err(format!("model '{}' offers unknown thinking effort '{e}'", m.alias));
                    }
                }
                if let Some(def) = m.thinking_effort.as_deref()
                    && !offered.iter().any(|e| e == def)
                {
                    return Err(format!(
                        "model '{}' default thinking effort '{def}' is not among its offered efforts",
                        m.alias
                    ));
                }
```

- [ ] **Step 8: Mirror the columns on model cards**

In `server/src/config/model_cards.rs`, extend `COLUMNS` with `thinking_efforts, default_thinking_effort, thinking_dialect`, and map them in the row reader and the insert/update statements using the same `decode_efforts`/`encode_efforts` helpers (re-export them from `store.rs` or duplicate them locally — they are four lines).

- [ ] **Step 9: Run tests to verify they pass**

Run: `cargo test -p horsie-server`
Expected: PASS, including the four new tests.

- [ ] **Step 10: Verify the migration against a populated DB**

```bash
python3 - <<'PY'
import sqlite3, os
db="/tmp/mig11-check.db"
os.path.exists(db) and os.remove(db)
c=sqlite3.connect(db)
c.executescript("""
CREATE TABLE models (alias TEXT PRIMARY KEY, provider TEXT NOT NULL, model_id TEXT NOT NULL, max_tokens INTEGER, context_window INTEGER);
CREATE TABLE model_cards (model_id TEXT PRIMARY KEY, name TEXT NOT NULL, context_window INTEGER, max_tokens INTEGER, created_at TEXT, updated_at TEXT);
INSERT INTO models VALUES ('kimi-k3','kimi','k3',32000,1000000);
INSERT INTO model_cards VALUES ('claude-sonnet-4-6','Claude Sonnet 4.6',200000,16384,'','');
""")
c.commit()
c.executescript(open("server/migrations/0011_thinking_efforts.sql").read())
c.commit()
print(list(c.execute("SELECT alias, thinking_efforts, thinking_effort, thinking_dialect FROM models")))
print(list(c.execute("SELECT model_id, thinking_efforts, default_thinking_effort, thinking_dialect FROM model_cards")))
PY
```

Expected: existing rows present with `None` in all new columns.

- [ ] **Step 11: Commit**

```bash
git add server/migrations/0011_thinking_efforts.sql models/fluorite/ server/src/config/ clients/web/src/generated/
git commit -m "feat(settings): per-model thinking efforts, default and dialect"
```

---

### Task 3: Provider dialect wiring and request translation

**Files:**
- Modify: `agentcore/src/provider.rs:5-11` (`CompletionRequest`)
- Modify: `agentcore/src/agent.rs:22-31` (`AgentConfig`), `:298-308` (request construction)
- Modify: `providers/anthropic/src/lib.rs` (dialect field + translation)
- Modify: `providers/openai/src/lib.rs`, `providers/openai/src/wire.rs:65-76` (`reasoning_effort`)
- Modify: `server/src/config/store.rs` (`build_anthropic` / `build_openai` gain the dialect)

**Interfaces:**
- Consumes: `ThinkingEffort`/`ThinkingDialect` (Task 1), `ModelRow.thinking_dialect` (Task 2), `async_llm::types::OutputConfig` (Plan B).
- Produces: `CompletionRequest.thinking_effort: Option<ThinkingEffort>`, `AgentConfig.thinking_effort: Option<ThinkingEffort>`, `AnthropicProvider::with_thinking_dialect`, `OpenAiProvider::with_thinking_dialect`.

- [ ] **Step 1: Write the failing test**

Append to the inline `mod tests` in `providers/anthropic/src/lib.rs`:

```rust
    fn effort(v: &str) -> ThinkingEffort {
        ThinkingEffort::parse(v).expect("canonical effort")
    }

    #[test]
    fn anthropic_effort_dialect_sets_output_config_and_adaptive() {
        let (thinking, output) = AnthropicProvider::encode_thinking(
            ThinkingDialect::AnthropicEffort,
            Some(effort("high")),
            None,
        );
        assert_eq!(
            output.expect("output_config set").effort.as_deref(),
            Some("high")
        );
        assert!(matches!(thinking, Some(ThinkingConfig::Enabled { .. }) | None));
    }

    #[test]
    fn anthropic_effort_dialect_maps_none_to_disabled() {
        let (thinking, output) = AnthropicProvider::encode_thinking(
            ThinkingDialect::AnthropicEffort,
            Some(effort("none")),
            None,
        );
        assert!(matches!(thinking, Some(ThinkingConfig::Disabled)));
        assert!(output.is_none(), "no effort field when thinking is disabled");
    }

    #[test]
    fn always_on_dialect_never_disables() {
        let (thinking, output) = AnthropicProvider::encode_thinking(
            ThinkingDialect::AnthropicAlwaysOn,
            Some(effort("max")),
            None,
        );
        assert!(thinking.is_none(), "thinking must be omitted entirely");
        assert_eq!(output.expect("set").effort.as_deref(), Some("max"));
    }

    #[test]
    fn budget_dialect_uses_budget_tokens() {
        let (thinking, _) = AnthropicProvider::encode_thinking(
            ThinkingDialect::AnthropicBudget,
            Some(effort("high")),
            Some(4096),
        );
        assert!(matches!(
            thinking,
            Some(ThinkingConfig::Enabled { budget_tokens: 4096 })
        ));
    }

    #[test]
    fn no_control_dialect_sends_nothing() {
        let (thinking, output) = AnthropicProvider::encode_thinking(
            ThinkingDialect::NoControl,
            Some(effort("high")),
            None,
        );
        assert!(thinking.is_none());
        assert!(output.is_none());
    }

    #[test]
    fn absent_effort_sends_nothing() {
        let (thinking, output) =
            AnthropicProvider::encode_thinking(ThinkingDialect::AnthropicEffort, None, None);
        assert!(thinking.is_none());
        assert!(output.is_none());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p horsie-anthropic encode_thinking`
Expected: FAIL to compile — `no function or associated item named 'encode_thinking'`.

- [ ] **Step 3: Implement the Anthropic translation**

Add to `impl AnthropicProvider`, next to `thinking_part`:

```rust
    /// Translate a canonical effort into this model's wire fields. Returns the
    /// `thinking` config and the `output_config`, either of which may be absent.
    fn encode_thinking(
        dialect: ThinkingDialect,
        effort: Option<ThinkingEffort>,
        budget_tokens: Option<u32>,
    ) -> (Option<ThinkingConfig>, Option<OutputConfig>) {
        let Some(effort) = effort else {
            return (None, None);
        };
        match dialect {
            ThinkingDialect::AnthropicEffort => {
                if effort.is_none_effort() {
                    (Some(ThinkingConfig::Disabled), None)
                } else {
                    (
                        None,
                        Some(OutputConfig {
                            effort: Some(effort.as_str().to_string()),
                        }),
                    )
                }
            }
            // Fable 5 rejects an explicit disable; `supports()` blocks `none`
            // at config time, so only effort values reach here.
            ThinkingDialect::AnthropicAlwaysOn => (
                None,
                Some(OutputConfig {
                    effort: Some(effort.as_str().to_string()),
                }),
            ),
            ThinkingDialect::AnthropicBudget => {
                if effort.is_none_effort() {
                    (Some(ThinkingConfig::Disabled), None)
                } else {
                    let thinking = budget_tokens
                        .map(|budget_tokens| ThinkingConfig::Enabled { budget_tokens });
                    (thinking, None)
                }
            }
            ThinkingDialect::ZaiThinking | ThinkingDialect::KimiThinking => {
                if effort.is_none_effort() {
                    (Some(ThinkingConfig::Disabled), None)
                } else {
                    (None, None)
                }
            }
            ThinkingDialect::OpenAiEffort | ThinkingDialect::NoControl => (None, None),
        }
    }
```

Import `OutputConfig` from `async_llm::types` and `ThinkingDialect`/`ThinkingEffort` from `horsie_agentcore` at the top of the file.

Add the field and builder, mirroring `keep_thinking_signature`:

```rust
    thinking_dialect: ThinkingDialect,
```

initialised to `ThinkingDialect::NoControl` in both constructors, with:

```rust
    #[must_use]
    pub fn with_thinking_dialect(mut self, dialect: ThinkingDialect) -> Self {
        self.thinking_dialect = dialect;
        self
    }
```

In `complete()`, replace the existing `if let Some(budget) = self.thinking_budget { ... }` block with:

```rust
        let (thinking, output_config) = Self::encode_thinking(
            self.thinking_dialect,
            req.thinking_effort,
            self.thinking_budget,
        );
        if let Some(t) = thinking {
            builder.thinking(t);
        }
        if let Some(oc) = output_config {
            builder.output_config(oc);
        }
```

- [ ] **Step 4: Implement the OpenAI translation**

In `providers/openai/src/wire.rs`, add to `ChatRequest`:

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
```

In `providers/openai/src/lib.rs`, add the same `thinking_dialect` field, constructor default, and `with_thinking_dialect` builder, and in `build_body()` set:

```rust
        reasoning_effort: match (self.thinking_dialect, req.thinking_effort) {
            (ThinkingDialect::OpenAiEffort, Some(e)) if !e.is_none_effort() => {
                Some(e.as_str().to_string())
            }
            (ThinkingDialect::OpenAiEffort, Some(_)) => Some("none".to_string()),
            _ => None,
        },
```

Add a matching unit test asserting that a non-`OpenAiEffort` dialect leaves `reasoning_effort` absent.

- [ ] **Step 5: Add the request and config fields**

`agentcore/src/provider.rs`:

```rust
pub struct CompletionRequest<'a> {
    pub messages: &'a [Message],
    pub system: Option<String>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    pub max_tokens: Option<u32>,
    /// Canonical thinking effort for this session; `None` sends no control.
    pub thinking_effort: Option<crate::thinking::ThinkingEffort>,
}
```

`agentcore/src/agent.rs`: add `pub thinking_effort: Option<ThinkingEffort>` to `AgentConfig` with `None` in its `Default` impl, and pass `thinking_effort: self.config.thinking_effort` in the `CompletionRequest` literal at `:298-308`.

Fix every other `CompletionRequest { ... }` construction the compiler names (test fixtures across the provider crates) by adding `thinking_effort: None`.

- [ ] **Step 6: Wire the dialect into the registry**

In `server/src/config/store.rs`, `build_anthropic` and `build_openai` each gain a `thinking_dialect: ThinkingDialect` parameter and call `.with_thinking_dialect(dialect)`. At the dispatch site, resolve it from the model row:

```rust
        let dialect = m
            .thinking_dialect
            .as_deref()
            .and_then(ThinkingDialect::parse)
            .unwrap_or(ThinkingDialect::NoControl);
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add agentcore/ providers/ server/src/config/store.rs
git commit -m "feat(providers): translate canonical thinking effort per model dialect"
```

---

### Task 4: Session-level selection

**Files:**
- Modify: `models/fluorite/session.fl:20-33` (`AgentSettings`)
- Modify: `server/src/sessions/spec.rs:32-48` (storage twin)
- Modify: `server/src/http/handlers.rs:59-70` (`settings_from_wire`) and the create handler
- Modify: `workflow/src/agent_actor.rs:1448` (`AgentConfig` construction)

**Interfaces:**
- Consumes: everything from Tasks 1-3.
- Produces: `AgentSettings.thinking_effort: Option<String>` on both wire and storage twins.

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `server/src/http/mod.rs`:

```rust
    #[tokio::test]
    async fn create_session_rejects_effort_the_model_does_not_offer() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let cfg = serde_json::json!({
            "providers": [{"name": "p", "kind": "anthropic", "baseUrl": "http://localhost:1", "apiKey": "sk-x"}],
            "models": [{
                "alias": "m", "provider": "p", "modelId": "id",
                "thinkingEfforts": ["none", "low"],
                "thinkingDialect": "anthropic_effort"
            }],
        });
        let res = app.clone().oneshot(put_json("/api/config", &cfg)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body = serde_json::json!({
            "agent": {"model": "m", "thinkingEffort": "max"}
        });
        let res = app.oneshot(put_json("/api/sessions", &body)).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "an effort outside the model's menu must be rejected at creation"
        );
    }
```

Adjust the create-session route/helper names to match the neighbouring session tests in that module (`create_list_get_message_lifecycle_over_http` shows the exact shape).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p horsie-server create_session_rejects_effort`
Expected: FAIL — the create succeeds (200/201) because nothing validates the field yet.

- [ ] **Step 3: Add the wire field**

In `models/fluorite/session.fl`, add to `AgentSettings`:

```
    /// Canonical thinking effort for this session, chosen from the model's
    /// offered list. Absent → the model's configured default.
    thinking_effort: Option<String>,
```

Regenerate TS: `cd clients/web && npm run generate-types && cd ../..`

- [ ] **Step 4: Add the storage twin field**

In `server/src/sessions/spec.rs`, add to `AgentSettings`:

```rust
    /// Canonical thinking effort chosen at session creation. `#[serde(default)]`
    /// so pre-thinking journal rows deserialize.
    #[serde(default)]
    pub thinking_effort: Option<String>,
```

and carry it in `settings_from_wire`:

```rust
        thinking_effort: w.thinking_effort,
```

- [ ] **Step 5: Validate at creation**

In the create-session handler in `server/src/http/handlers.rs`, after the model is resolved and before the session is spawned:

```rust
    if let Some(requested) = settings.thinking_effort.as_deref() {
        let Some(effort) = horsie_agentcore::ThinkingEffort::parse(requested) else {
            return Err(Api::unprocessable(format!(
                "unknown thinking effort '{requested}'"
            )));
        };
        let offered = state.config.model_thinking_efforts(&settings.model).await;
        if !offered.iter().any(|e| e == effort.as_str()) {
            return Err(Api::unprocessable(format!(
                "model '{}' does not offer thinking effort '{requested}'",
                settings.model
            )));
        }
    }
```

Add `model_thinking_efforts(&self, alias: &str) -> Vec<String>` to the `ConfigStore` trait and implement it on `DbConfigStore` by reading the model row. Use whatever error constructor the neighbouring handlers use for 422 — match the existing `Api::` helper rather than inventing one.

- [ ] **Step 6: Resolve the effective effort**

In `workflow/src/agent_actor.rs` at the `AgentConfig` construction (`:1448`), resolve session value → model default → none, and set `thinking_effort` on the config. The model's default is read from the same registry lookup that already resolves the provider.

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add models/fluorite/session.fl server/src/sessions/spec.rs server/src/http/ workflow/src/agent_actor.rs clients/web/src/generated/
git commit -m "feat(sessions): choose thinking effort at session creation"
```

---

## Verification

- [ ] `cargo test --workspace` passes (baseline 724 + new tests).
- [ ] `cargo clippy --workspace --all-targets` clean.
- [ ] `cd clients/web && npx tsc -b` clean after regeneration.
- [ ] Migration `0011` applies to a populated DB, all new columns NULL on existing rows.
- [ ] A session created without `thinkingEffort` against a model with no thinking config produces a request byte-identical to today's (no `thinking`, no `output_config`, no `reasoning_effort`).

## Deferred to Plan D

- Web UI: the session config bar control and the settings model form fields.
- The model-card reseed (~38 cards with efforts, defaults and dialects).
