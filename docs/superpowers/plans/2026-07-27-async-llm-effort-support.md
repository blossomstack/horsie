# async-llm Effort Support Implementation Plan (Plan B)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give async-llm the request-side fields horsie needs to control thinking depth, and repoint horsie at `main`.

**Architecture:** async-llm is Anthropic-Messages-only, so it gains exactly two things: an `output_config` object carrying `effort` (the modern Anthropic control), and an optional-on-serialize `signature` so a stripped thinking block is *omitted* rather than sent as `""`. horsie then moves its git pin from the stale `horsie-pinned-async-llm` branch to `main`.

**Tech Stack:** Rust, serde, `derive_builder`, wiremock (async-llm's existing test harness).

## Global Constraints

- **Do not touch `src/client.rs` in the user's checkout.** It carries an uncommitted `TEMP DIAGNOSTIC` patch (reads the response body out of `reqwest_eventsource::Error::InvalidStatusCode`). All async-llm work happens in a **separate worktree** so the user's dirty main is untouched.
- async-llm repo: `/Users/xiaoguang/works/repos/bloomstack/october/async-llm`, remote `git@github.com:blossomstack/async-llm.git`, base branch `main` (`a7e720e`).
- **No merge of `horsie-pinned-async-llm` is required.** Verified 2026-07-27: `publish.yml` and `Cargo.toml` are byte-identical between `a7e720e` and `97cac01`; the only differences across the other 9 files are rustfmt import ordering and line wrapping. `main` is a squashed redo of the same work.
- Edition 2021, no `rustfmt.toml`. Do **not** run `cargo fmt` across the repo — history shows mixed stable/nightly formatting and a blanket reformat would bury the real diff.
- horsie pins async-llm by git `rev`, not by crates.io version. A version bump alone changes nothing for horsie.
- Never list Claude as author/co-author on commits.

---

### Task 1: Omit the thinking signature instead of sending `""`

Closes the known limitation carried by Plan A.

**Files:**
- Modify: `src/types.rs:58-64` (`Thinking`)
- Test: `tests/messages.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Thinking.signature: Option<String>` (breaking for callers that assign a bare `String`; horsie's only assignment site is `providers/anthropic/src/lib.rs:231`, updated in Task 3).

- [ ] **Step 1: Create the isolated worktree**

```bash
cd /Users/xiaoguang/works/repos/bloomstack/october/async-llm
git fetch origin
git worktree add /tmp/async-llm-effort -b feat/effort-support origin/main
cd /tmp/async-llm-effort
git status --short   # must be empty — the user's dirty client.rs stays in their checkout
```

- [ ] **Step 2: Write the failing test**

Append to `tests/messages.rs`:

```rust
#[test]
fn thinking_signature_is_omitted_when_absent() {
    use async_llm::types::Thinking;
    let block = Thinking {
        thinking: "reasoning".into(),
        signature: None,
        cache_control: None,
    };
    let json = serde_json::to_value(&block).expect("serializes");
    assert_eq!(json["thinking"], "reasoning");
    assert!(
        json.get("signature").is_none(),
        "signature must be omitted, not sent as an empty string: {json}"
    );
}

#[test]
fn thinking_signature_round_trips_when_present() {
    use async_llm::types::Thinking;
    let block = Thinking {
        thinking: "reasoning".into(),
        signature: Some("sig-blob".into()),
        cache_control: None,
    };
    let json = serde_json::to_value(&block).expect("serializes");
    assert_eq!(json["signature"], "sig-blob");

    let back: Thinking = serde_json::from_value(json).expect("deserializes");
    assert_eq!(back.signature.as_deref(), Some("sig-blob"));
}

#[test]
fn thinking_deserializes_without_signature() {
    use async_llm::types::Thinking;
    let block: Thinking =
        serde_json::from_value(serde_json::json!({"thinking": "reasoning"})).expect("deserializes");
    assert_eq!(block.signature, None);
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --test messages thinking_signature`
Expected: FAIL to compile — `expected String, found Option<String>` (or an assertion failure on the omitted-signature test, depending on compile order).

- [ ] **Step 4: Write the implementation**

In `src/types.rs`, replace the `signature` field of `Thinking`:

```rust
pub struct Thinking {
    pub thinking: String,
    /// Provider replay signature. `None` when the provider did not supply one,
    /// or when the caller deliberately dropped it — genuine Anthropic validates
    /// this on replay, but Anthropic-compatible endpoints generally do not.
    /// Omitted from the request when `None`; an empty string is *not* a valid
    /// substitute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}
```

- [ ] **Step 5: Fix in-crate compile errors**

Run: `cargo build --all-targets`
Any in-crate construction of `Thinking { signature: ... }` now needs `Some(...)`. Fix each site the compiler names; do not change behaviour beyond the wrapping.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS, including the three new tests.

- [ ] **Step 7: Commit**

```bash
git add src/types.rs tests/messages.rs
git commit -m "fix: omit thinking signature when absent instead of sending empty string"
```

---

### Task 2: Add `output_config.effort`

**Files:**
- Modify: `src/types.rs:152-189` (`CreateMessagesRequest`), plus a new `OutputConfig` type alongside `ThinkingConfig`
- Test: `tests/messages.rs`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `OutputConfig { effort: Option<String> }` and `CreateMessagesRequest.output_config: Option<OutputConfig>`, settable via `CreateMessagesRequestBuilder::output_config(...)`. Consumed by Plan C's Anthropic adapter.

- [ ] **Step 1: Write the failing test**

Append to `tests/messages.rs`:

```rust
#[test]
fn output_config_effort_serializes_under_output_config() {
    use async_llm::types::{CreateMessagesRequestBuilder, MessageBuilder, MessageRole, OutputConfig};

    let request = CreateMessagesRequestBuilder::default()
        .model("claude-opus-4-8".to_string())
        .messages(vec![
            MessageBuilder::default()
                .role(MessageRole::User)
                .content("hi")
                .build()
                .expect("message builds"),
        ])
        .output_config(OutputConfig {
            effort: Some("high".into()),
        })
        .build()
        .expect("request builds");

    let json = serde_json::to_value(&request).expect("serializes");
    assert_eq!(json["output_config"]["effort"], "high");
}

#[test]
fn output_config_is_omitted_when_unset() {
    use async_llm::types::{CreateMessagesRequestBuilder, MessageBuilder, MessageRole};

    let request = CreateMessagesRequestBuilder::default()
        .model("claude-opus-4-8".to_string())
        .messages(vec![
            MessageBuilder::default()
                .role(MessageRole::User)
                .content("hi")
                .build()
                .expect("message builds"),
        ])
        .build()
        .expect("request builds");

    let json = serde_json::to_value(&request).expect("serializes");
    assert!(
        json.get("output_config").is_none(),
        "output_config must be absent when unset: {json}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test messages output_config`
Expected: FAIL to compile — `no method named 'output_config'` and `cannot find type 'OutputConfig'`.

- [ ] **Step 3: Add the type**

In `src/types.rs`, immediately after the `ThinkingConfig` enum:

```rust
/// Output-shaping controls. Currently just `effort`, which sets reasoning
/// depth on models that support it (`low` / `medium` / `high` / `xhigh` /
/// `max` on current Anthropic models; the accepted set is model-specific, so
/// this is an unvalidated string and the caller owns the vocabulary).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OutputConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}
```

- [ ] **Step 4: Add the request field**

In `CreateMessagesRequest`, after the existing `thinking` field:

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    #[builder(default)]
    pub output_config: Option<OutputConfig>,
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit and push**

```bash
git add src/types.rs tests/messages.rs
git commit -m "feat: add output_config.effort to the messages request"
git push -u origin feat/effort-support
```

Open a PR against `main` and merge it. Record the resulting `main` commit SHA — Task 3 needs it.

---

### Task 3: Repoint horsie at async-llm `main`

**Files:**
- Modify: `Cargo.toml:37` (horsie worktree `.horsie/worktrees/thinking-config`)
- Modify: `providers/anthropic/src/lib.rs:231` (drop `unwrap_or_default`)

**Interfaces:**
- Consumes: `Thinking.signature: Option<String>` (Task 1), `OutputConfig` (Task 2).
- Produces: a horsie build pinned to async-llm `main`.

- [ ] **Step 1: Move the pin**

In the horsie worktree, replace the `rev` in `Cargo.toml:37` with the merged `main` SHA from Task 2:

```toml
async-llm         = { version = "0.7.0", git = "https://github.com/blossomstack/async-llm.git", rev = "<new main SHA>" }
```

- [ ] **Step 2: Stop flattening `None` into `""`**

In `providers/anthropic/src/lib.rs`, the `ContentPart::Thinking` arm of `parts_to_api_content` currently reads `signature: th.signature.clone().unwrap_or_default()`. With Task 1 the field is already `Option<String>`, so it becomes a straight move:

```rust
                ContentPart::Thinking(th) => MessageContent::Thinking(Thinking {
                    thinking: th.text.clone(),
                    signature: th.signature.clone(),
                    ..Default::default()
                }),
```

- [ ] **Step 3: Add a regression test**

Append to the inline `mod tests` in `providers/anthropic/src/lib.rs`:

```rust
    #[test]
    fn parts_to_api_content_omits_absent_thinking_signature() {
        let parts = vec![ContentPart::Thinking(ThinkingPart {
            text: "reasoning".into(),
            signature: None,
        })];
        let list = AnthropicProvider::parts_to_api_content(&parts);
        let json = serde_json::to_value(&list[0]).expect("serializes");
        assert!(
            json.get("signature").is_none(),
            "an absent signature must not be sent as an empty string: {json}"
        );
    }
```

- [ ] **Step 4: Verify the workspace**

```bash
cargo update -p async-llm
cargo test --workspace
cargo clippy --workspace --all-targets
```

Expected: build succeeds against the new rev; test count is at least the 724 recorded on 2026-07-27 plus the new test; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock providers/anthropic/src/lib.rs
git commit -m "chore: pin async-llm to main; omit absent thinking signatures"
```

- [ ] **Step 6: Clean up the worktree and the stale branch**

```bash
cd /Users/xiaoguang/works/repos/bloomstack/october/async-llm
git worktree remove /tmp/async-llm-effort
```

Deleting the now-unreferenced `horsie-pinned-async-llm` branch (local and remote) is **a decision for the user, not this plan** — it is the only ref recording what horsie shipped against before this change. Ask before deleting.

---

## Verification

- [ ] async-llm `cargo test` passes in the worktree.
- [ ] The user's `/Users/xiaoguang/works/repos/bloomstack/october/async-llm` checkout still shows exactly one modified file, `src/client.rs`, with the 21-line TEMP DIAGNOSTIC patch intact.
- [ ] horsie `cargo test --workspace` and `cargo clippy --workspace --all-targets` pass against the new rev.
- [ ] A request built without `output_config` serializes without the key (no behaviour change for existing callers).
