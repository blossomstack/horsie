# Workflow step model and session-mode design

## Goal

A workflow execution is not a conversation with a representative agent. The opened step page must render the selected execution's configuration (for example `deepseek-v4-flash` for `code`), never the start step's (`gpt-5.6-terra`) settings. The persisted server model must make that distinction structural: workflow sessions cannot possess a synthetic session-wide `AgentSettings`.

Old serialized `SessionSpec` compatibility is intentionally **not** retained. This change may alter the serde shape of session specs; do not add aliases, defaults, or migrations to read the removed `agent` / `workflow` combination.

## Current failure path

`control/workflows.rs::run_workflow` resolves each step's preset correctly, then copies the start step into a wire `AgentSettings` solely to call `build_session_spec`. `SessionSpec` subsequently retains that value in `agent`, while its optional `workflow` snapshot holds all actual step settings. `handlers::detail` exposes `spec.agent` as `SessionDetail.model`, and `SessionView` gives the locked configuration bar that session document even when the route is scoped to a step. Thus the UI displays the start-step model.

The actor's `read_agent` already looks up a step execution's model from its workflow snapshot, but the HTTP `AgentDocument` currently drops it. Subagents are worse: their actor plan and concurrency limit are copied from `spec.agent`, so a workflow step's subagents inherit the synthetic start-step settings.

## Domain design

### 1. Replace the invalid `agent + workflow` pairing

In `crates/server/src/sessions/spec.rs`, replace the session-wide `agent: AgentSettings` field and optional `workflow: Option<Arc<WorkflowRunSpec>>` with one tagged domain enum, named for the repository's conventions (recommended: `SessionKind`):

```rust
enum SessionKind {
    Agent { settings: AgentSettings },
    Workflow { run: Arc<WorkflowRunSpec> },
}
```

Keep common session infrastructure (`name`, runtime/vendor/environment, provision, plugin bundle union, env vars) directly on `SessionSpec`. Restrict `SessionOrigin` to `User` and `Routine`; a workflow is identified by `SessionKind::Workflow`, so `SessionOrigin::Workflow` is redundant and must go. `routine()`, `is_unattended()`, and a `workflow_name()` projection should match these domain values exhaustively.

Add narrow accessors on `SessionSpec` / `SessionActor` rather than scattering enum matches:

- agent-session settings, only when a main agent can exist;
- workflow-run snapshot, only for workflow sessions;
- effective settings for an `AgentKey` / `SubAgentParent`.

Effective settings must resolve as follows:

- `Main` and `Fork` use the `Agent` variant's settings;
- `Step(id)` finds the execution in `SessionState.run`, then its named `WorkflowStepSpec` in the `Workflow` snapshot;
- `Sub(id)` inherits its recorded parent recursively, ending at the owning main agent or workflow step.

An absent/malformed runtime record must fail the relevant operation rather than silently falling back to a session default; the type must not reintroduce one. This accessor is the one source used by read, spawn, and limit logic.

Update every constructor/fixture/recovery test to explicitly choose `SessionKind::Agent` or `SessionKind::Workflow`. `build_session_spec` remains the builder for user, agent-preset, and routine runs and returns `SessionKind::Agent`; workflow creation builds its shared environment/provision values without fabricating wire or storage `AgentSettings`, then installs `SessionKind::Workflow { run }`.

### 2. Make actor behavior mode-aware through the type

Update `SessionActor` initialization and all former `spec.agent` / `spec.workflow` consumers:

- spawn a main agent only for `SessionKind::Agent`;
- construct the workflow orchestrator only from `SessionKind::Workflow`;
- use the effective-settings accessor to create step agents and readable cold agents;
- use it for fork plans (forks are valid only under an agent session) and reject/avoid impossible workflow-main/fork paths;
- replace the synthetic main-agent model in usage reporting with an optional/main-agent-only value, or remove that unused projection if no consumer needs it. Workflow graph usage continues to use its per-execution usage map;
- update workflow HTTP graph lookup and all supervisor/actor test fixtures to get the run through the `Workflow` variant.

For subagents, preserve the existing tree ownership and global active-count behavior, but apply the calling agent's effective settings:

- `SubAgentCommand::Spawn` obtains the caller's settings before checking `max_subagents()`;
- `spawn_sub_agent_actor` derives the new node's plan from its stored parent, so cold recovery and descendants inherit the same step-specific settings;
- workflow-step subagents must inherit the step's model, MCP/memory/tool/plugin/thinking/budget settings, not the workflow start step's old placeholder.

### 3. Put agent configuration on the agent protocol, not the session protocol

Edit the fluorite schemas, then regenerate committed TypeScript output with `make types` (do not manually edit generated files):

- Remove model and all agent-scoped configuration from `session.SessionDetail`: `model`, MCP servers, memory spaces, plugin enablement, and thinking effort. Retain only genuinely session/runtime-wide facts such as environment/vendor/repos, the run-wide provisioned plugin bundle union, usage total, roster, forks, and workflow marker.
- Extend `session_api.AgentDocument` from the actor's `AgentDetail` with the configuration the locked UI presents: model plus MCP servers, memory spaces, plugin enablement, and thinking effort. In the Rust actor, carry the resolved effective `AgentSettings` in `AgentDetail` rather than only a model string; the HTTP handler maps that to the wire document and looks up `context_window` using that document's model.
- Update schema comments, server type imports, generated Rust use sites, and generated TypeScript fixtures to reflect that `AgentDocument` is per-agent (main, step, or subagent) and `SessionDetail` makes no session-wide-model claim.

### 4. Render locked configuration for the opened agent

Change `SessionConfigBar` / `useLockedChannels` to receive both the session infrastructure document and the selected `AgentDocument`. It should source environment/repos/plugin bundle facts from `SessionDetail`, but model, MCP, memory, plugin enablement, and thinking readouts from `AgentDocument`. `SessionView` already calls `useAgent(id, agentId ?? MAIN_AGENT)`, so pass that selected document into the locked bar and wait for it before rendering agent-dependent controls. This automatically makes `/sessions/:id/agents/:step-id` show the opened step's configuration.

Preserve the behavior for ordinary sessions and forks: their selected document resolves to the main/fork effective agent settings. A workflow run root still routes to `WorkflowRunView`; it has no configuration bar claiming a main model.

## File-level implementation map

- `crates/server/src/sessions/spec.rs`: introduce `SessionKind`, remove the synthetic fields/origin arm, add projections, and revise serialization/unit fixtures. Do not retain old serde compatibility for the removed specification shape.
- `crates/server/src/sessions/builder.rs`, `control/workflows.rs`, `control/agents.rs`, `routines/runner.rs`, runtime-manager and supervisor/HTTP test builders: construct the appropriate explicit kind. Remove start-step-to-wire-settings construction in workflow launch.
- `crates/server/src/sessions/session_actor/{mod.rs,reads.rs,run.rs,subagent.rs,fork.rs,types.rs,testing.rs}`: centralize effective-settings lookup; make main/step spawn, agent detail, usage, subagent cap/spawn/recovery, and fixtures exhaustive over the new kind.
- `crates/server/src/http/{handlers.rs,workflows.rs,mod.rs}`: project the new protocol shapes, use per-agent model/config, and retrieve workflow snapshots from the kind variant.
- `crates/models/fluorite/{session.fl,session_api.fl}` plus regenerated `clients/web/src/generated/**`: protocol change.
- `clients/web/src/{pages/SessionView.tsx,components/SessionConfigBar.tsx,components/configPickers.tsx}` and their tests: compose session-level and selected-agent data for locked controls.

## Focused tests and verification

### Rust

1. Add/update `spec.rs` unit coverage for projections and serialization of both explicit session kinds. The test fixtures must demonstrate there is no workflow constructor with a session agent; do not add legacy deserialization tests for the removed shape.
2. Add actor/read coverage using a two-step snapshot with different settings (`plan` = `gpt-5.6-terra`, `code` = `deepseek-v4-flash`). Resolve/read the code execution and assert its `AgentDetail` configuration is Flash; assert a subagent spawned from that execution receives the same effective settings and its cap is evaluated from the code step.
3. Update HTTP handler tests to assert `AgentDocument` carries the selected agent's configuration/context window and `SessionDetail` has no model/config fields. Update workflow graph tests for the new snapshot accessor.

Run focused server tests with the required feature unification, e.g. `cargo test -p horsie-server --features test-util <relevant test filters>`, then `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --workspace` as appropriate for the broad domain/protocol change.

### Web

1. Update `SessionConfigBar.test.tsx` fixtures so session details contain only session facts and agent documents contain model/configuration.
2. Add the regression test at the locked-bar/session-view boundary: render a workflow session detail whose historic/start step is Terra and an opened `code` `AgentDocument` set to `deepseek-v4-flash`; assert the Model key/readout says Flash and never Terra. Include another step-specific setting readout (such as thinking or MCP) to prove the bar follows the selected agent configuration rather than session data.
3. Run `make types`, then the focused Vitest test(s), `bun run test` (or the repository's configured web test command), and `bun run build` from `clients/web` / `make web-build`.

Finally commit all product/protocol/generated/test changes on `fix/workflow-step-model`, push it, open a conventional PR (recommended title: `fix(workflows): use step settings for workflow agents`), and wait for every required GitHub check to pass and the PR to be mergeable. Fix CI failures on the branch before reporting completion.
