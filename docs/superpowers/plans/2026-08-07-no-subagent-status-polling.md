# Eliminate Subagent Status Polling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the model from polling `subagent_status` after delegating work while preserving the tool for exceptional progress inspection and diagnosis.

**Architecture:** Update the three model-facing instruction surfaces: the base session prompt, the subagent-role suffix, and both delegation tool descriptions. The existing session actor already injects terminal subagent outcomes, so no lifecycle or API changes are required. Protect the wording with focused Rust unit tests at the prompt/tool-spec seams.

**Tech Stack:** Rust, Tokio unit tests, `horsie_agentcore::ToolSpec`.

## Global Constraints

- Keep `subagent_status` available with its existing name, schema, visibility, and output behavior.
- Do not change session-actor subagent persistence or automatic terminal-result delivery.
- State that results and failures are delivered automatically and that status polling/repeated calls are prohibited.
- Allow `subagent_status` only for a user-requested progress report or diagnosis of suspected runtime/result-delivery problems.

---

## File structure

- `server/src/sessions/session_actor/system_prompt.md` owns the general delegation policy sent to every session agent.
- `server/src/sessions/session_actor/context.rs` owns the extra system-prompt role guidance for spawned subagents and its assembly tests.
- `server/src/sessions/spawn_tool.rs` owns the `spawn_agent` and `subagent_status` model-visible `ToolSpec` descriptions and toolbox tests.

### Task 1: Make delegation guidance prohibit status polling

**Files:**
- Modify: `server/src/sessions/session_actor/system_prompt.md:99-104`
- Modify: `server/src/sessions/session_actor/context.rs:147-153`
- Test: `server/src/sessions/session_actor/context.rs:747-786`

**Interfaces:**
- Consumes: `SUBAGENT_PROMPT_SUFFIX`, appended by `SessionContextProvider::provide()` for `SessionAgentKind::Sub`.
- Produces: main-agent and subagent system prompts that describe automatic terminal-result delivery and exceptional-only status inspection.

- [ ] **Step 1: Add a failing assertion for the subagent role prompt**

In `subagent_toolbox_strips_session_metadata_tools`, retain the existing role-heading assertion and add checks against the unwrapped prompt:

```rust
let prompt = sub.system_prompt.unwrap();
assert!(prompt.contains("automatically delivered"), "{prompt}");
assert!(prompt.contains("Do not poll"), "{prompt}");
assert!(prompt.contains("user requests a progress update"), "{prompt}");
```

- [ ] **Step 2: Run the focused test and verify it fails**

Run:

```bash
cargo test -p horsie-server subagent_toolbox_strips_session_metadata_tools
```

Expected: FAIL because the existing suffix says to “check on them with subagent_status”.

- [ ] **Step 3: Replace main-agent delegation guidance**

In `system_prompt.md`, replace the paragraph that ends with “Check progress with `subagent_status`” with text that says results or failures are automatically delivered, directs the agent to continue independent work or wait when none remains, prohibits polling, and limits status inspection to a user-requested progress update or suspected delivery/runtime problem.

- [ ] **Step 4: Replace the subagent-role suffix**

Change `SUBAGENT_PROMPT_SUFFIX` so its sub-spawning sentence uses the same policy: outcomes are automatically delivered, the subagent must not poll or repeat status calls, and it may call `subagent_status` only for a user-requested progress update or suspected runtime/result-delivery problem.

- [ ] **Step 5: Run the focused test and verify it passes**

Run:

```bash
cargo test -p horsie-server subagent_toolbox_strips_session_metadata_tools
```

Expected: PASS.

- [ ] **Step 6: Commit the prompt guidance**

```bash
git add server/src/sessions/session_actor/system_prompt.md server/src/sessions/session_actor/context.rs
git commit -m "fix: discourage subagent status polling"
```

### Task 2: Describe the tools as asynchronous and exceptional-only

**Files:**
- Modify: `server/src/sessions/spawn_tool.rs:26-109`
- Test: `server/src/sessions/spawn_tool.rs:291-360`

**Interfaces:**
- Consumes: private `spawn_agent_spec(&AgentCatalog) -> ToolSpec` and `subagent_status_spec() -> ToolSpec`.
- Produces: unchanged tool names and input schemas with descriptions that instruct the model not to poll.

- [ ] **Step 1: Add failing tool-description tests**

In `spawn_tool.rs`'s existing `mod tests`, add a unit test that builds both specs using `AgentCatalog::default()` and asserts:

```rust
let spawn = spawn_agent_spec(&AgentCatalog::default());
assert!(spawn.description.contains("automatically delivered"), "{}", spawn.description);
assert!(spawn.description.contains("Do not poll"), "{}", spawn.description);

let status = subagent_status_spec();
assert!(status.description.contains("user-requested progress update"), "{}", status.description);
assert!(status.description.contains("diagnos"), "{}", status.description);
assert!(status.description.contains("Do not poll"), "{}", status.description);
```

Name the test `tool_descriptions_prohibit_status_polling`.

- [ ] **Step 2: Run the focused test and verify it fails**

Run:

```bash
cargo test -p horsie-server tool_descriptions_prohibit_status_polling
```

Expected: FAIL because `spawn_agent_spec` currently says “Use subagent_status to check progress” and the status spec calls itself “Check on subagents”.

- [ ] **Step 3: Update `spawn_agent_spec` description**

Keep its asynchronous and limit-error documentation. Replace the instruction to check progress with wording that completion and failure are automatically delivered as a message, tells the agent to continue independent work or wait, and says never to poll or repeatedly call `subagent_status`.

- [ ] **Step 4: Update `subagent_status_spec` description**

Retain its `id` and subtree behavior. Describe the tool as exceptional-only: use it for a user-requested progress update or to diagnose a suspected runtime/result-delivery problem; do not poll or call it repeatedly because terminal outcomes are automatically delivered.

- [ ] **Step 5: Run the focused test and verify it passes**

Run:

```bash
cargo test -p horsie-server tool_descriptions_prohibit_status_polling
```

Expected: PASS.

- [ ] **Step 6: Commit tool guidance and tests**

```bash
git add server/src/sessions/spawn_tool.rs
git commit -m "fix: document exceptional subagent status checks"
```

### Task 3: Verify the completed change

**Files:**
- Verify: `server/src/sessions/session_actor/context.rs`
- Verify: `server/src/sessions/spawn_tool.rs`
- Verify: `server/src/sessions/session_actor/system_prompt.md`

**Interfaces:**
- Consumes: completed prompt and `ToolSpec` changes from Tasks 1–2.
- Produces: formatting- and test-verified no-poll delegation guidance.

- [ ] **Step 1: Format the Rust changes**

Run:

```bash
cargo fmt --check
```

Expected: PASS. If it reports formatting changes, run `cargo fmt`, then re-run `cargo fmt --check`.

- [ ] **Step 2: Run focused server tests**

Run:

```bash
cargo test -p horsie-server subagent_toolbox_strips_session_metadata_tools
cargo test -p horsie-server tool_descriptions_prohibit_status_polling
```

Expected: both focused tests PASS.

- [ ] **Step 3: Run the workspace test suite**

Run:

```bash
cargo test --workspace
```

Expected: PASS.

- [ ] **Step 4: Inspect the final diff**

Run:

```bash
git diff origin/main...HEAD --check
git status --short
```

Expected: no whitespace errors and a clean working tree.

- [ ] **Step 5: Commit any formatting-only changes**

If `cargo fmt` modified tracked files:

```bash
git add <formatted-files>
git commit -m "style: format subagent delegation guidance"
```
