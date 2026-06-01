# Live Skill Loading Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `skill(name)` fetch live (re-scan on each call), add an always-present `list_skills` tool that returns the current catalog, drop the spawn-time skill cache and the `RwLock`-free swap, and revert `ToolboxFactory::for_agent` to `(agent_def, runtime_client)`.

**Architecture:** `AgentToolbox` holds only a `RuntimeClient`; `skill` and `list_skills` are always advertised and re-scan the workspace over the runtime on each call (no cached `SkillSet`, no lock). `spawn_agent` still scans once to compose the `# Available skills` prompt block.

**Tech Stack:** Rust (edition 2024), tokio, the existing `workspace::scan` over `RuntimeClient`. No new dependencies.

**Reference spec:** `docs/superpowers/specs/2026-05-31-live-skill-loading-design.md`

**Conventions:** Production code denies `unwrap`/`expect`/`panic`/`wildcard_enum_match_arm`; test modules opt out with the standard `#[allow(...)]` block. Commit messages: conventional, succinct, no AI attribution. This refactors skill-tool code already on branch `skills-support` (PR #35, unmerged).

---

### Task 1: Shared skills rendering helpers in `workspace.rs`

**Files:**
- Modify: `workflow/src/workspace.rs`

- [ ] **Step 1: Add `len()` to `SkillSet`**

In the `impl SkillSet` block (next to `is_empty`/`get`/`names`), add:

```rust
    pub fn len(&self) -> usize {
        self.skills.len()
    }
```

- [ ] **Step 2: Add the rendering helpers**

Add these free functions to `workflow/src/workspace.rs` (after `compose_system_prompt`):

```rust
/// Render skills as sorted `- name: description` lines. Shared by the prompt's
/// `# Available skills` block and the `list_skills` tool result.
fn skills_listing(skills: &SkillSet) -> String {
    skills
        .iter()
        .map(|s| format!("- {}: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `list_skills` tool result: a count + catalog, or an empty-state line.
pub(crate) fn list_skills_result(skills: &SkillSet) -> String {
    if skills.is_empty() {
        "No skills found in the workspace.".to_string()
    } else {
        format!("{} skills available:\n{}", skills.len(), skills_listing(skills))
    }
}
```

- [ ] **Step 3: Refactor `compose_system_prompt` to use `skills_listing`**

Replace the skills block in `compose_system_prompt` (the `if !ws.skills.is_empty() { ... }` arm) with:

```rust
    if !ws.skills.is_empty() {
        sections.push(format!(
            "# Available skills\nLoad a skill's full instructions with the `skill` tool before relying on it.\n{}",
            skills_listing(&ws.skills)
        ));
    }
```

- [ ] **Step 4: Add tests for the helpers**

Add to the `#[cfg(test)] mod tests` in `workspace.rs`:

```rust
    #[test]
    fn list_skills_result_lists_or_reports_empty() {
        let empty = SkillSet::default();
        assert_eq!(list_skills_result(&empty), "No skills found in the workspace.");

        let set = SkillSet::from_iter([
            Skill { name: "a".into(), description: "first".into(), body: "x".into() },
            Skill { name: "b".into(), description: "second".into(), body: "y".into() },
        ]);
        let out = list_skills_result(&set);
        assert!(out.starts_with("2 skills available:\n"));
        assert!(out.contains("- a: first"));
        assert!(out.contains("- b: second"));
    }
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p workflow workspace::`
Expected: all pass, including `list_skills_result_lists_or_reports_empty` and the existing `compose_*` tests (the prompt output is unchanged).

- [ ] **Step 6: Commit**

```bash
git add workflow/src/workspace.rs
git commit -m "refactor(workflow): factor skills_listing + list_skills_result helpers"
```

---

### Task 2: Live `AgentToolbox` + `list_skills` + revert `for_agent` signature

This changes the `ToolboxFactory::for_agent` signature, so every implementor and caller updates in the same commit to stay buildable.

**Files:**
- Modify: `workflow/src/context.rs` (trait, factory, `AgentToolbox`, tests)
- Modify: `workflow/src/workflow_actor.rs` (`spawn_agent` call site)
- Modify: `workflow/src/lib.rs` (export `LIST_SKILLS_TOOL`)
- Modify: `workflow/tests/workflow_e2e.rs` (`BlockingFactory`)
- Modify: `workflow/tests/workspace_context.rs` (integration test)

- [ ] **Step 1: Add the `LIST_SKILLS_TOOL` constant**

In `workflow/src/context.rs`, next to `pub const SKILL_TOOL: &str = "skill";`, add:

```rust
/// Name of the builtin tool that re-scans the workspace and returns the current
/// skill catalog (name + description). Always advertised, like `skill`.
pub const LIST_SKILLS_TOOL: &str = "list_skills";
```

- [ ] **Step 2: Revert the `ToolboxFactory` trait signature**

Change the trait in `context.rs` to drop the `skills` parameter:

```rust
pub trait ToolboxFactory: Send + Sync + 'static {
    fn for_agent(&self, agent_def: &WorkflowAgentDef, runtime_client: RuntimeClient) -> Arc<dyn Toolbox>;
}
```

- [ ] **Step 3: Update `DefaultToolboxFactory::for_agent` to keep the client**

```rust
impl ToolboxFactory for DefaultToolboxFactory {
    fn for_agent(&self, agent_def: &WorkflowAgentDef, runtime_client: RuntimeClient) -> Arc<dyn Toolbox> {
        let client = runtime_client.clone();
        let runtime = add_runtime_tools(ToolboxImpl::new(), runtime_client);
        let base: Arc<dyn Toolbox> = match &agent_def.allowed_tools {
            None => Arc::new(runtime),
            Some(list) => Arc::new(FilteredToolbox::new(
                Arc::new(runtime),
                list.iter().cloned().collect(),
            )),
        };
        let conclude =
            conclude_tool_spec(agent_def.output_schema.as_ref(), agent_def.allow_ask_user);
        Arc::new(AgentToolbox {
            base,
            conclude,
            runtime_client: client,
        })
    }
}
```

- [ ] **Step 4: Replace `AgentToolbox` struct + impl with the live version**

Replace the `AgentToolbox` struct (the `skills: Arc<SkillSet>` field) and its `Toolbox` impl with:

```rust
/// A toolbox = a base (permitted runtime tools), the optional `conclude` terminal
/// tool, and the always-present `skill` / `list_skills` tools. The latter two re-scan
/// the workspace live on each call (no cached skill set), so a skill added mid-run is
/// immediately loadable. `conclude`, `skill`, and `list_skills` bypass the allowlist.
struct AgentToolbox {
    base: Arc<dyn Toolbox>,
    conclude: Option<ToolSpec>,
    runtime_client: RuntimeClient,
}

#[async_trait]
impl Toolbox for AgentToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.base.specs();
        if let Some(c) = &self.conclude {
            specs.push(c.clone());
        }
        specs.push(ToolSpec {
            name: SKILL_TOOL.to_string(),
            description:
                "Load the full instructions for a named skill (see 'Available skills' or list_skills)."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["name"],
                "properties": { "name": { "type": "string", "description": "The skill name." } }
            }),
        });
        specs.push(ToolSpec {
            name: LIST_SKILLS_TOOL.to_string(),
            description:
                "Re-scan the workspace and list the skills currently available (name + description)."
                    .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        });
        specs
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        if let Some(c) = &self.conclude
            && name == c.name
        {
            return Err(ToolCallError::ExecutionFailed(
                "the conclude tool is terminal and is not executed".to_string(),
            ));
        }
        if name == SKILL_TOOL {
            let requested = input.get("name").and_then(Value::as_str).unwrap_or_default();
            let ws = crate::workspace::scan(&self.runtime_client).await;
            return match ws.skills.get(requested) {
                Some(skill) => Ok(Value::String(skill.body.clone())),
                None => Err(ToolCallError::InvalidInput(format!(
                    "unknown skill '{requested}'; available: {}",
                    ws.skills.names().join(", ")
                ))),
            };
        }
        if name == LIST_SKILLS_TOOL {
            let ws = crate::workspace::scan(&self.runtime_client).await;
            return Ok(Value::String(crate::workspace::list_skills_result(&ws.skills)));
        }
        self.base.execute(name, input).await
    }
}
```

- [ ] **Step 5: Fix the `context.rs` import**

`AgentToolbox` no longer references `SkillSet`. Remove the now-unused `use crate::workspace::SkillSet;` import at the top of `context.rs` (the tests re-import what they need). Keep everything else.

- [ ] **Step 6: Rewrite the `context.rs` skill-tool tests**

Replace the two skill tests (`skill_tool_advertised_and_serves_body`, `no_skill_tool_when_empty`) and fix the two `for_agent` calls in the older tests. The skill set now comes from the client's scan, not a passed-in `Arc<SkillSet>`.

Add a scan helper near the top of the `context.rs` test module:

```rust
    fn scan_with_skill() -> models::runtime::WorkspaceScan {
        models::runtime::WorkspaceScan {
            instructions: None,
            skills: vec![models::runtime::ScannedFile {
                path: ".claude/skills/git-bisect/SKILL.md".into(),
                content: "---\nname: git-bisect\ndescription: find bad commit\n---\nStep 1...".into(),
            }],
        }
    }
```

Update the existing two tests' `for_agent` calls to two arguments:

```rust
        let tb = DefaultToolboxFactory.for_agent(&def(Some(vec!["bash".into()]), Some(out), false), client);
```
and
```rust
        let tb = DefaultToolboxFactory.for_agent(&def(None, Some(out), false), client);
```

Replace the two skill-specific tests with:

```rust
    #[tokio::test]
    async fn skill_and_list_skills_always_present() {
        let client = RuntimeClient::new(MockTransport::ok("")); // empty scan
        let tb = DefaultToolboxFactory.for_agent(&def(None, None, false), client);
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&SKILL_TOOL.to_string()));
        assert!(names.contains(&LIST_SKILLS_TOOL.to_string()));
    }

    #[tokio::test]
    async fn skill_fetches_live_and_list_skills_reports() {
        let client = RuntimeClient::new(MockTransport::ok("").with_scan(scan_with_skill()));
        let tb = DefaultToolboxFactory.for_agent(&def(None, None, false), client);

        let body = tb.execute(SKILL_TOOL, json!({ "name": "git-bisect" })).await.unwrap();
        assert_eq!(body, json!("Step 1..."));

        let err = tb.execute(SKILL_TOOL, json!({ "name": "nope" })).await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));

        let listed = tb.execute(LIST_SKILLS_TOOL, json!({})).await.unwrap();
        assert_eq!(listed, json!("1 skills available:\n- git-bisect: find bad commit"));
    }
```

(`use crate::workspace::SkillSet;` is no longer needed in the test module — remove it if present. `Arc` may become unused in tests; remove the `use std::sync::Arc;` test import if the compiler flags it.)

- [ ] **Step 7: Update `spawn_agent` (`workflow_actor.rs`)**

In `WorkflowActor::spawn_agent`, drop the third argument to `for_agent` (keep the scan for the prompt):

```rust
        // Scan once to compose the `# Available skills` prompt block; the toolbox
        // fetches skills live on its own.
        let ws = crate::workspace::scan(&self.rt.runtime_client).await;
        let toolbox = self
            .rt
            .toolbox_factory
            .for_agent(agent_def, self.rt.runtime_client.clone());
```

The `params.system_prompt = crate::workspace::compose_system_prompt(...)` line below it is unchanged.

- [ ] **Step 8: Update `BlockingFactory` (`workflow/tests/workflow_e2e.rs`)**

Change its `for_agent` to the two-argument signature (drop the `_skills` param):

```rust
impl ToolboxFactory for BlockingFactory {
    fn for_agent(&self, def: &WorkflowAgentDef, _client: RuntimeClient) -> Arc<dyn Toolbox> {
        let conclude = conclude_tool_spec(def.output_schema.as_ref(), def.allow_ask_user)
            .expect("worker has an output schema");
```

(Remove the now-unused `workflow::SkillSet` reference / `Arc<workflow::SkillSet>` param. Leave the rest of the impl unchanged.)

- [ ] **Step 9: Export `LIST_SKILLS_TOOL` from `lib.rs`**

In `workflow/src/lib.rs`, add `LIST_SKILLS_TOOL` to the `pub use context::{ ... }` list (next to `SKILL_TOOL`).

- [ ] **Step 10: Update the integration test (`workflow/tests/workspace_context.rs`)**

The `for_agent` calls drop their third argument, and the "no skill tool when empty" assertion flips to "always present". Replace both tests' bodies' `for_agent(...)` calls and the empty-state assertion:

```rust
    // in scan_composes_prompt_and_exposes_skill_tool:
    let tb = DefaultToolboxFactory.for_agent(&agent_def(), client);
    let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"bash".to_string()));
    assert!(names.contains(&"skill".to_string()));
    assert!(names.contains(&"list_skills".to_string()));
    let body = tb.execute("skill", serde_json::json!({ "name": "git-bisect" })).await.unwrap();
    assert_eq!(body, serde_json::json!("Run git bisect."));
```

```rust
    // in empty_workspace_yields_plain_prompt_and_no_skill_tool (rename intent: tools always present):
    let tb = DefaultToolboxFactory.for_agent(&agent_def(), client);
    let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"skill".to_string()));
    assert!(names.contains(&"list_skills".to_string()));
```

(`ws.skills` is no longer passed anywhere; the `let ws = scan_workspace(&client).await;` line stays only where the prompt is asserted. Remove any now-unused binding the compiler flags.)

- [ ] **Step 11: Build and test the workflow crate**

Run: `cargo test -p workflow`
Expected: all pass — `context::` unit tests, `workspace::` tests, and both integration test files.

- [ ] **Step 12: Commit**

```bash
git add workflow/src/context.rs workflow/src/workflow_actor.rs workflow/src/lib.rs workflow/tests/workflow_e2e.rs workflow/tests/workspace_context.rs
git commit -m "feat(workflow): live skill loading + list_skills tool; drop skill cache"
```

---

### Task 3: Full verification + update the PR

**Files:** none (verification only)

- [ ] **Step 1: Run the combined gate**

```bash
set -e
cargo build --workspace >/dev/null 2>&1; echo "build ok"
cargo clippy --all-targets --all-features -- -D warnings >/dev/null 2>&1; echo "clippy ok"
cargo fmt --check >/dev/null 2>&1; echo "fmt ok"
cargo test --workspace >/tmp/test.log 2>&1; echo "test ok"
cargo deny check >/tmp/deny.log 2>&1; echo "deny ok"
echo "GATE_GREEN"
```
Expected: prints `GATE_GREEN`. If clippy flags an unused import (`SkillSet`, `Arc`) in a changed file, remove it and re-run.

- [ ] **Step 2: Push**

```bash
git push
```

- [ ] **Step 3: Watch CI to green**

```bash
gh run watch "$(gh run list --branch skills-support --limit 1 --json databaseId -q '.[0].databaseId')" --exit-status --interval 10
```
Expected: both the `Check` job (Format/Clippy/Tests on Rust 1.96.0) and `Supply chain (cargo-deny)` pass. If anything fails, read the log, fix, and re-push.

---

## Self-Review

**Spec coverage:**
- `skill(name)` live → Task 2 Step 4 (execute re-scans).
- `list_skills` tool → Task 2 Step 4 + Task 1 (`list_skills_result`).
- Both always present, bypass allowlist → Task 2 Step 4 (`specs()` always pushes; layered above `FilteredToolbox`).
- No cached state / no `RwLock` → Task 2 Step 4 (`AgentToolbox { base, conclude, runtime_client }`).
- `for_agent` reverts to 2 args → Task 2 Steps 2–3, 7, 8, 10.
- Empty-state = tool descriptions only → Task 1 Step 3 keeps the prompt block omitted when empty; no prompt note added.
- Shared `skills_listing` → Task 1.

**Placeholder scan:** No TBD/placeholders; every code step is complete. Line-number-free anchors reference named functions/structs.

**Type consistency:** `for_agent(agent_def, runtime_client)` is consistent across trait, `DefaultToolboxFactory`, `BlockingFactory`, `spawn_agent`, and both test files. `AgentToolbox` fields (`base`, `conclude`, `runtime_client`) match construction in Step 3. `list_skills_result`/`skills_listing` signatures match their call sites. `LIST_SKILLS_TOOL`/`SKILL_TOOL` constants used consistently.
