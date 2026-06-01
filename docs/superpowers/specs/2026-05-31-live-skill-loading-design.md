# Live skill loading + `list_skills` tool

- **Date:** 2026-05-31
- **Status:** design, pending implementation
- **Builds on:** `2026-05-31-workspace-context-loading-design.md` (same PR #35 / branch `skills-support`, not yet merged)

## Goal

Let an agent pick up skills that appear or change *during* its turn (a `git pull`,
or an earlier step writing a skill), and make skill loading always available — even
when the workspace had zero skills at spawn. Do this with **no cached skill state**
in the toolbox.

## Motivation

The parent feature scans skills at spawn, caches the bodies, and advertises a `skill`
tool only when that cached set is non-empty. Two limits follow: (1) the cache is
frozen for the agent's turn, so a mid-run change is invisible; (2) if there were no
skills at spawn, the `skill` tool is absent and never reappears.

`agentcore` re-reads `toolbox.specs()` on every loop iteration (`agentcore/src/agent.rs:253`),
so a tool can be present every turn without any per-turn rebuild. That makes an
always-present, live-fetching design natural.

## Design

### Behavior

- **`skill(name)` fetches live.** On each call it re-scans the workspace over the
  runtime and returns the named skill's body — always current, never cached.
- **New `list_skills` tool.** Re-scans and returns the current catalog
  (`name: description` per line) so the agent learns what exists now. The system
  prompt's `# Available skills` block is frozen for the turn, so this is how a
  mid-turn change reaches the agent — via the tool result, not the prompt.
- **Both tools are always advertised**, regardless of whether skills exist at spawn,
  and are **not** subject to `allowed_tools` (like `conclude`).
- **No cached skill state.** The toolbox holds only a `RuntimeClient`. No `SkillSet`
  field, no lock, no swap.

### Why not a cached set + reload-and-swap

An earlier draft cached bodies at spawn and had a "reload" tool mutate that cache
(`Arc<RwLock<Arc<SkillSet>>>` + swap). Live fetching removes the cache entirely, so
there is nothing to mutate — simpler, always fresh, and it lets `for_agent` shed the
`Arc<SkillSet>` parameter. The cost is a runtime scan per `skill()`/`list_skills()`
call (each transfers all skill bodies to pick one); negligible for typical skill
counts, optimizable later if a workspace ever carries dozens.

### Components

`AgentToolbox` (in `workflow/src/context.rs`):

```rust
struct AgentToolbox {
    base: Arc<dyn Toolbox>,
    conclude: Option<ToolSpec>,
    runtime_client: RuntimeClient,   // was: skills: Arc<SkillSet>
}
```

- `specs()` (sync, per-iteration): `base.specs()` + optional `conclude` + the two
  static specs `skill` and `list_skills`. No scan, no lock.
- `execute("skill", { name })`:
  `let ws = workspace::scan(&self.runtime_client).await;`
  `ws.skills.get(name)` → `Ok(Value::String(body))`, else
  `InvalidInput("unknown skill '{name}'; available: {names}")`.
- `execute("list_skills", {})`:
  `let ws = workspace::scan(&self.runtime_client).await;`
  `Ok(Value::String(skills_listing(&ws.skills)))`.
- `conclude` interception is unchanged.

Tool constants in `context.rs`: keep `SKILL_TOOL = "skill"`; add
`LIST_SKILLS_TOOL = "list_skills"`.

`workspace.rs`: factor the `name: description` rendering into a shared
`skills_listing(&SkillSet) -> String` helper, used by both `compose_system_prompt`
(under the `# Available skills` header) and the `list_skills` result. `list_skills`
on an empty set returns `"No skills found in the workspace."`.

### Signature revert

`ToolboxFactory::for_agent` goes back to `(agent_def, runtime_client)` — the
`Arc<SkillSet>` parameter added by the parent feature is removed. `for_agent` clones
the `RuntimeClient` (Arc-backed) so both `add_runtime_tools` and `AgentToolbox` hold
one. `WorkflowActor::spawn_agent` still scans once to compose the `# Available skills`
prompt block via `compose_system_prompt`, but no longer threads skills into the
toolbox. Other implementors (`BlockingFactory` in `workflow/tests/workflow_e2e.rs`)
revert to the two-argument signature.

`compose_system_prompt` keeps its current behavior: lists skills at spawn when
non-empty, omits the section when empty (empty-state discoverability is via the tool
descriptions only — no prompt note).

### Prompt / catalog shape

System prompt at spawn (unchanged, when skills exist):
```
# Available skills
Load a skill's full instructions with the `skill` tool before relying on it.
- git-bisect: Find the commit that introduced a regression
```

`list_skills()` result:
```
2 skills available:
- git-bisect: Find the commit that introduced a regression
- pdf-fill: Fill PDF forms by field name
```

## Edge cases

- **Scan transport failure** (e.g. server/relay mode, which has no scan command yet):
  `workspace::scan` already degrades to an empty context with a warning, so
  `skill(name)` returns `InvalidInput` ("no skills") and `list_skills` returns
  "No skills found." — consistent with the existing distributed-mode limitation.
- **Skill removed since spawn**: `skill(name)` on it returns `InvalidInput`.
- **Skill added since spawn**: immediately loadable; `list_skills` shows it.

## Testing

- `skill` and `list_skills` are advertised even when the spawn scan returns no skills.
- `skill(name)` returns the body from a live scan (`MockTransport::ok("").with_scan(...)`).
- `list_skills` returns the catalog; empty set → "No skills found."
- Build the toolbox with a client whose scan returns a skill, assert `skill(name)`
  serves it with no skills threaded in at construction (demonstrates the cache is gone).
- Update `context.rs` unit tests and the `workflow/tests/workspace_context.rs`
  integration test that previously asserted "no skill tool when empty" → now both
  tools are always present.

## Out of scope

Reloading instructions (`AGENTS.md`) — skills only; targeted single-file fetch
optimization; wiring scan through the executor relay (still the open follow-up).
