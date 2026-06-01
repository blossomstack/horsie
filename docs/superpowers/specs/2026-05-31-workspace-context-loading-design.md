# Workspace context loading: AGENTS.md + skills

- **Date:** 2026-05-31
- **Status:** design, pending implementation
- **Scope:** load a workspace instruction file (`AGENTS.md`) and progressive-disclosure
  *skills* from the runtime's working directory, and surface them to every agent.

## Goal

When october runs a workflow against a workdir, agents should pick up that
workspace's project instructions and skills — the same way Claude Code reads
`CLAUDE.md` and `.claude/skills/`. Concretely:

1. The first found of `AGENTS.md` / `AGENT.md` / `CLAUDE.md` at the workdir root is
   injected into every agent's system prompt as "workspace context".
2. Skills under `.claude/skills/<name>/SKILL.md` are advertised by `name` +
   `description` in the prompt; the full body is loaded on demand via a synthesized
   `skill` tool (progressive disclosure).

## Decisions

These were settled during brainstorming and are not open:

| # | Decision | Choice |
|---|----------|--------|
| 1 | Skill consumption | **Progressive disclosure** — metadata in prompt, body via `skill` tool |
| 2 | File conventions | **Mirror Claude Code** — `AGENTS.md`/`AGENT.md`/`CLAUDE.md`; `.claude/skills/<name>/SKILL.md` |
| 3 | Multi-agent scope | **All agents always** (no per-agent opt-out in v1) |
| 4 | Where the scan runs | **In the runtime, over `RuntimeClient`** — never host-fs, so future runtime providers work unchanged |
| 5 | Scan transport | **Dedicated `ScanWorkspace` protocol op** (parameterized request, raw file contents back) |
| 6 | Integration point | **Every agent's system prompt**; `skill` tool serves bodies from cache |
| 7 | Freshness | **Re-scan on every agent spawn** (planner→coder→reviewer and loop re-entries) |

The driving rationale for #4/#5: the workspace lives in the runtime's filesystem.
A future remote/containerized runtime provider has no host-side directory to scan,
so the scan must be a runtime operation issued once the runtime is ready.

The driving rationale for #7: an agent's `system_prompt` is fixed for the whole
duration of its turn (baked into `AgentParams` at spawn, reused every iteration),
so the advertised skill list can only be as fresh as "when this agent started."
That makes per-spawn the natural refresh boundary, and it catches the common
mid-run mutations — a `git pull`/`checkout` or an earlier agent editing skills.

## Conventions (client-owned policy)

Defined as constants in `workflow/src/workspace.rs`, passed to the runtime in the
scan request so the runtime stays convention-agnostic:

```rust
const INSTRUCTION_CANDIDATES: &[&str] = &["AGENTS.md", "AGENT.md", "CLAUDE.md"];
const SKILLS_GLOB: &str = ".claude/skills/*/SKILL.md";
```

`.claude` is a literal in the glob pattern, so the `glob` crate matches the hidden
directory; the `*` only spans the (non-hidden) skill-name directories.

## Architecture

### Data flow

```
WorkflowActor::spawn_agent(agent_def)           runtime subprocess (sandboxed, in workdir)
  workspace = workspace::scan(&runtime_client) ── ScanWorkspace(ScanRequest) ──▶ scan::exec
                                          ◀──────── ScanResult(WorkspaceScan) ───  glob + read
  system_prompt = compose(agent_def.system_prompt, &workspace)
  toolbox       = factory.for_agent(agent_def, runtime_client, workspace.skills)
  spawn AgentActor with the composed params + toolbox
```

`run.rs` / `drive()` are **unchanged** — the feature lives entirely behind the
existing `RuntimeClient` the workflow actor already holds.

### 1. Protocol additions — `fluorite/runtime.fl`

The op is parameterized (client supplies candidates + glob) and returns raw file
contents; frontmatter parsing stays client-side so the runtime remains a pure
executor.

```
// inbound
struct ScanRequest {
    call_id: String,
    instruction_candidates: Vec<String>,
    skills_glob: String,
}
union RuntimeInboundMessage {
    ToolCall(ToolCallRequest),
    CancelCall(CancelCallRequest),
    ScanWorkspace(ScanRequest)
}

// outbound
struct ScannedFile  { path: String, content: String }
struct WorkspaceScan { instructions: Option<ScannedFile>, skills: Vec<ScannedFile> }
struct ScanResponse  { call_id: String, scan: WorkspaceScan }
union RuntimeOutboundMessage {
    Ready(RuntimeReady),
    ToolCallResponse(ToolCallResponse),
    ScanResult(ScanResponse)
}
```

The scan has **no error variant** — it is best-effort. A missing instruction
candidate → `instructions = None`; an unreadable skill file → omitted from
`skills`. Transport death remains a `TransportError` at the transport layer.

> **fluorite gotcha:** do not put `///` doc comments on union *variants* — it
> silently breaks `.fl` codegen (missing `OUT_DIR/<pkg>/mod.rs`). Comment the
> structs above the union instead.

After editing the schema, regenerate: `cargo build -p models`.

### 2. Runtime side

- `runtime/src/main.rs::run_loop`: add a `RuntimeInboundMessage::ScanWorkspace(req)`
  match arm that spawns a task → `scan::exec(&working_dir, req)` → sends back
  `RuntimeOutboundMessage::ScanResult(ScanResponse { call_id, scan })`. Mirror the
  existing `ToolCall` arm (it already has `working_dir`, `sink`, `call_id` in scope).
- New `runtime/src/scan.rs`:
  - instructions: try each candidate via `std::fs::read_to_string`, first `Ok` wins
    → `Some(ScannedFile{ path, content })`, else `None`.
  - skills: run the `glob` crate on `{working_dir}/{skills_glob}`, read each match;
    a read error on an individual file is skipped (best-effort).
  - Runs inside `working_dir`, covered by the existing WorkingDir capability grant —
    **no capability changes**.

### 3. Transport + `RuntimeClient`

Scan responses carry a different payload than `ToolCallResponse`, so the socket
transport needs a parallel correlation path:

- `RuntimeTransport` trait (`runtime-client/src/transport.rs`):
  `async fn scan_workspace(&self, call_id: &str, candidates: Vec<String>, skills_glob: String) -> Result<WorkspaceScan, TransportError>`.
- `SocketRuntimeTransport` (`executor/src/socket_transport.rs`): add a second pending
  map `pending_scan: Mutex<HashMap<String, oneshot::Sender<WorkspaceScan>>>`; the
  reader demuxes `RuntimeOutboundMessage::ScanResult` → `pending_scan`, leaving the
  `ToolCallResponse` path untouched.
- `MockTransport` (test transport in `runtime-client`): trivial impl returning a
  preset `WorkspaceScan`.
- `RuntimeClient::scan_workspace(&self, candidates, skills_glob) -> Result<WorkspaceScan, RuntimeCallError>`
  mirrors `invoke` (generates the `call_id`).

### 4. Client interpretation — `workflow/src/workspace.rs` (new)

```rust
pub struct WorkspaceContext { pub instructions: Option<String>, pub skills: Arc<SkillSet> }
pub struct SkillSet { skills: BTreeMap<String, Skill> }   // sorted name → stable prompt
pub struct Skill { pub name: String, pub description: String, pub body: String }

/// Issue the scan and interpret it. On transport error: warn + empty context.
pub async fn scan(client: &RuntimeClient) -> WorkspaceContext;

/// Role-first prompt assembly; omit empty sections.
pub fn compose_system_prompt(agent_prompt: Option<&str>, ws: &WorkspaceContext) -> Option<String>;
```

- Frontmatter: split `---\n…\n---\n<body>`; parse the fenced block as YAML into
  `{ name: String, description: String }` (ignore unknown keys). Use a YAML parser
  (add `serde_yaml` or an existing workspace yaml dep — verify it passes
  `cargo-deny`'s license allowlist). The map key is the frontmatter `name`; the
  directory name is ignored (mismatch → `warn`). Duplicate `name` → keep first, warn.
- Malformed / missing frontmatter, or missing `name`/`description` → skip that skill
  with `tracing::warn!`, never fail the scan.
- `Skill.body` is the content after the closing `---` (the metadata is already in
  the prompt; the tool returns just the instructions).

### 5. Prompt composition

Role first, environment after, sections omitted when empty:

```
<agent_def.system_prompt>

# Workspace context
<AGENTS.md contents>

# Available skills
Load a skill's full instructions with the `skill` tool before relying on it.
- pdf-fill: Fill PDF forms by field name
- git-bisect: Find the commit that introduced a regression
```

An agent with no `system_prompt` and a populated workspace gets just the workspace
block; an empty workspace yields `None` (unchanged behavior).

### 6. The `skill` tool

Synthesized at the toolbox layer exactly like `conclude`:

- Added to `AgentToolbox` when `SkillSet` is non-empty, **not** subject to
  `allowed_tools` (consistent with decision #3 and with how `conclude` bypasses the
  allowlist).
- `spec`: `name: "skill"`, `input_schema: { name: string (required) }`.
- `execute("skill", { name })` → `Ok(Value::String(body))` from the cached `SkillSet`;
  unknown name → `ToolCallError::InvalidInput` listing valid names. The terminal
  `conclude` interception is unchanged.
- Skills that reference bundled files (`references/x.md`) are reachable via the normal
  `read_file` tool, since `.claude/skills/...` is inside the workdir.

### 7. Wiring changes

- `workflow/src/context.rs`:
  - `ToolboxFactory::for_agent` gains an `Arc<SkillSet>` parameter; `DefaultToolboxFactory`
    layers the skill tool into `AgentToolbox` (which now also holds `Arc<SkillSet>`).
  - `WorkflowRuntimeContext` is **unchanged** (no cached workspace; it already carries
    `runtime_client`).
- `workflow/src/workflow_actor.rs`:
  - `spawn_agent` becomes `async`; callers (`on_start`, transition handling) `.await` it.
  - Body: `let ws = workspace::scan(&self.rt.runtime_client).await;`
    compose the prompt, build the toolbox with `ws.skills.clone()`, set the composed
    prompt onto `AgentParams`.
- `workflow/src/agent_actor.rs`: `AgentParams` already holds `system_prompt: Option<String>`;
  set the composed value at spawn (e.g. `AgentParams::from_def(def).with_system_prompt(prompt)`
  or compose inline in `spawn_agent`). No change to `run_with_retries`.

## Failure handling & resume

- **Scan transport failure** during a spawn → `warn` + empty `WorkspaceContext` for
  that agent; the run proceeds (the feature is additive and must not sink a run).
- **Missing files** → absent sections, feature simply inactive.
- **Malformed skill** → that skill skipped with a warning; others still load.
- **Resume**: `drive()` re-runs against a fresh runtime; the first spawn after resume
  re-scans, so edited `AGENTS.md`/skills are picked up. Nothing about the scan is
  journaled (it is runtime wiring, recomputed each spawn).

## Testing

- `runtime/src/scan.rs` (tempdir): instruction precedence (`AGENTS.md` wins over
  `CLAUDE.md`), skills glob, missing instruction file → `None`, unreadable skill skipped.
- `workflow/src/workspace.rs`: frontmatter parse (valid; missing fence skipped; missing
  `name`/`description` skipped; duplicate name kept-first); `compose_system_prompt`
  (role-first ordering, sections omitted when empty, agent-prompt-absent case); skill
  tool `execute` hit/miss.
- `runtime-client`: `MockTransport` scan + `RuntimeClient::scan_workspace` round-trip.
- `executor/src/socket_transport.rs`: a `ScanResult` is routed to `pending_scan` and
  does not disturb in-flight tool calls.
- e2e (`workflow` or `cli` `tests/`): run a workflow against a workdir containing
  `AGENTS.md` + one skill; assert the agent's prompt includes the workspace block and
  the skill metadata, and that `skill(name)` returns the body.

## Out of scope (YAGNI)

User/global skills (`~/.claude/skills`), plugin skills, nested/home `CLAUDE.md`
merging, per-agent opt-out, config-overridable paths (the `ScanRequest` fields make
this a later one-liner), live per-call body fetch, hot-reload within a single agent
turn.

## Touched files (summary)

- `fluorite/runtime.fl` — `ScanRequest`, `ScannedFile`, `WorkspaceScan`,
  `ScanResponse`, two new union variants.
- `runtime/src/main.rs`, `runtime/src/scan.rs` (new) — dispatch + scan impl.
- `runtime-client/src/transport.rs`, `runtime-client/src/client.rs` — trait method +
  client method; `MockTransport` impl.
- `executor/src/socket_transport.rs` — `pending_scan` map + reader demux.
- `workflow/src/workspace.rs` (new) — `WorkspaceContext`/`SkillSet`/`Skill`, `scan`,
  `compose_system_prompt`, skill tool.
- `workflow/src/context.rs` — `for_agent` signature + `AgentToolbox` skill layer.
- `workflow/src/workflow_actor.rs` — async `spawn_agent` scanning per spawn.
- `models` — regenerated from the schema.
