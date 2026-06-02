# Multiple workspaces per job: named roots, scoped skills, qualified tools

- **Date:** 2026-06-01
- **Status:** design, pending implementation
- **Scope:** let one job run against **several co-equal workspace roots** instead of a
  single `workdir`. Each root is a *named* workspace; skills, instruction files, and
  filesystem/exec tools are all addressed **by workspace name**. Builds directly on
  [2026-05-31-workspace-context-loading-design.md](2026-05-31-workspace-context-loading-design.md)
  and [2026-05-31-live-skill-loading-design.md](2026-05-31-live-skill-loading-design.md).

## Goal

A user editing several related repos in one session (e.g. an API, its shared types,
and a frontend) should be able to point a single job at all of them:

```bash
agentx run --workdir ./services/api,./services/web,../shared "wire api into web"
```

and have every agent see — for **each** workspace — its instructions, its skills, and
whether it's a git repo, then act on a specific workspace by name. There is **no
implicit "primary" workspace and no default cwd**: a filesystem/exec tool call names
the workspace it targets (optional only when there is exactly one workspace, which
keeps all current single-root behavior unchanged).

## Decisions

Settled during brainstorming; not open:

| # | Decision | Choice |
|---|----------|--------|
| 1 | Workspace model | **Co-equal named roots** (VS Code multi-root shape), not a primary + add-dirs |
| 2 | Naming | **Derived from path**: basename, lengthened with parent segments on conflict; identical paths → error |
| 3 | Declaration | **Comma-separated `--workdir`** at job creation; no manifest file, no explicit `name=path` override |
| 4 | Skills | **Grouped per workspace, never merged**; collisions only matter within a workspace (kept-first, as today) |
| 5 | Tool targeting | **Explicit `workspace` field** on fs/exec tools; **no default cwd**. Optional ⇔ exactly one workspace |
| 6 | Sandbox | **One `WorkingDir` grant expands to every root**; no per-root distinction, no `CapabilitySpec` schema change |
| 7 | Per-workspace metadata | Surface **path, `is_git_repo`, instructions, skills** in the prompt, one block per workspace |
| 8 | Distributed/relay mode | **Out of scope** (relay scan is already a known gap); multi-workspace widens it but doesn't address it |

**Why no default cwd (#5):** a privileged "root[0]" makes a bare relative path resolve
somewhere invisible and position-dependent, which is exactly the future confusion we
want to avoid. Forcing each fs/exec call to name its workspace makes the location
explicit and is symmetric with workspace-scoped skills. Per the project's "make
illegal states unrepresentable" philosophy, *where* an action happens is part of the
call, not ambient state.

**Why derive names (#2):** zero-config for the caller; the agent never *constructs* a
name (it reads the `name → path` table from the prompt), so the only requirement is
uniqueness + human-meaningfulness. Lengthening-on-conflict is position-dependent (adding
a second `api` repo renames the first from `api` to `services/api`), accepted because
the prompt always shows the current mapping within a session.

## Naming algorithm

A pure, testable function (no I/O):

```rust
// models/src/lib.rs (hand-written)
pub struct Workspace { pub name: String, pub path: PathBuf }
pub fn derive_workspaces(paths: &[PathBuf]) -> Result<Vec<Workspace>, WorkspaceError>;
```

1. Candidate name = last path component.
2. While any two candidates collide, prepend the next parent segment to **each**
   colliding candidate (joined with `/`, e.g. `api` → `services/api`).
3. Repeat until all unique. Two byte-identical paths → `Err(WorkspaceError::Duplicate)`.

| Input paths | Names |
|---|---|
| `./api`, `./web`, `../shared` | `api`, `web`, `shared` |
| `./services/api`, `./tools/api` | `services/api`, `tools/api` |
| `/a/x/api`, `/b/x/api` | `a/x/api`, `b/x/api` |

Names are derived **once at submit time** and persisted in `JobSpec.workspaces`, so a
resumed job keeps stable names regardless of code changes to the derivation.

## Architecture

### Data flow (changed pieces in **bold**)

```
agentx run --workdir a,b,c
  → SubmitRequest { workdirs: ["a","b","c"], .. }          (daemon wire)
  → daemon: derive_workspaces(["a","b","c"])               ← naming happens here, once
  → JobSpec { workspaces: Vec<Workspace>, .. }             (storage)
  → RuntimeConfig { workspaces: Vec<Workspace>, .. }
  → process_provider: runtime --workspace name=path (×N)   (repeatable arg)
  → sandbox::apply(&[paths]) — one WorkingDir grant → every root
  → ScanWorkspace ⇒ Vec<WorkspaceScan> (one per workspace, labeled by name)
  → compose_system_prompt: a "# Workspaces" block per workspace
  → tool calls carry workspace: Option<String>; runtime resolves name → path
```

### Where & when the scan runs (unchanged from single-root)

The scan is a **runtime-side filesystem operation**, not a host-side one — the
workspace lives in the runtime's (sandboxed) filesystem, and a future remote runtime
has no host directory to read. The host/client side only *interprets* raw bytes
(frontmatter → `SkillSet`); it does no I/O. The actual reads happen in
`runtime/src/scan.rs::exec`, behind the sandbox that already grants every root (§3).

It travels over the existing dedicated `ScanWorkspace` op:

```
spawn_agent / skill tool                         runtime subprocess (sandboxed)
  workspace::scan(&runtime_client)
    RuntimeClient::scan_workspace(candidates, glob)
      ── ScanWorkspace(ScanRequest) ──▶  run_loop arm (main.rs)
                                           scan::exec(&registry, req)   ← fs reads + .git check
      ◀── ScanResult(ScanResponse) ──     per root
  interpret(Vec<WorkspaceScan>) → WorkspaceContext
```

It fires at two points, both **live and uncached** (so mid-run edits / `git pull` are
seen): (1) **every agent spawn** — re-scans before composing that agent's prompt; and
(2) **every `skill` / `list_skills` call** — since the prompt is frozen at spawn, the
tools' live re-scan is how freshness is recovered within a turn. Multi-workspace keeps
both triggers and the single round-trip; only `scan::exec`'s body fans out over roots.

### 1. Naming + storage types

- `models::Workspace { name: String, path: PathBuf }` + `derive_workspaces` (above).
  Hand-written in `models/src/lib.rs` (a path-naming helper, not a wire message), with
  `Serialize`/`Deserialize` so `JobSpec` can persist it.
- `supervisor/src/spec.rs`: `JobSpec.workdir: PathBuf` → `workspaces: Vec<Workspace>`
  (must be non-empty). This is the persisted source of truth for the named set.

### 2. CLI + daemon wire

- `cli/src/main.rs`: `--workdir` parses a comma-separated list into `Vec<PathBuf>`
  (a value with no comma = a single workspace — fully backward compatible). Document
  the one caveat: paths can't contain commas.
- `fluorite/daemon.fl` `SubmitRequest`: `workdir: String` → `workdirs: Vec<String>`
  (raw paths; the daemon, not the CLI, derives names so derivation lives in one place).
- `cli/src/daemon/mod.rs`: on submit, `derive_workspaces(&paths)?` → `JobSpec.workspaces`.
  A duplicate-path error is reported to the user at submit time (fail fast).

### 3. Runtime invocation + sandbox

- `executor/src/process_provider.rs`: replace the single `--working-dir` with a
  repeatable `--workspace <name>=<path>` (one per workspace from `config.workspaces`).
- `runtime/src/main.rs`: `--working-dir: PathBuf` → `--workspace: Vec<String>`, each
  parsed `name=path` into a `Workspace`. The runtime builds a **`WorkspaceRegistry`**
  (`BTreeMap<String, PathBuf>`, name → absolute path) held for the process lifetime and
  threaded into `dispatch` and `scan`.
- `runtime/src/sandbox.rs`: `apply(working_dir: &Path, ..)` →
  `apply(working_dirs: &[PathBuf], ..)`. The single `Grant::WorkingDir` arm loops over
  every root, calling `allow_path` once per root with the granted access. **No
  `CapabilitySpec` schema change** — `WorkingDirGrant` simply means "every workspace
  root" now. One rule, all workspaces (per decision #6).

### 4. Protocol: per-workspace scan — `fluorite/runtime.fl`

`ScanRequest` gains a `workspace: Option<String>` **filter** so the runtime — the one
place that holds the `WorkspaceRegistry` — does all name→path translation (for the scan
*and* the fs/exec tools, against the same registry). `WorkspaceScan` becomes
**per-workspace** and the response carries a list:

```
struct ScanRequest {                                              // + workspace filter
    call_id: String,
    workspace: Option<String>,                                    // None = all roots
    instruction_candidates: Vec<String>,
    skills_glob: String,
}
struct ScannedFile  { path: String, content: String }            // unchanged
struct WorkspaceScan {                                            // now per-workspace
    name: String,
    path: String,                                                 // absolute root path
    is_git_repo: Bool,
    instructions: Option<ScannedFile>,
    skills: Vec<ScannedFile>,
}
struct ScanResponse { call_id: String, workspaces: Vec<WorkspaceScan> }
```

`workspace`: `None` → scan every root (the spawn-time prompt scan); `Some(name)` →
resolve via the registry and return just that root; an unknown name → empty
`workspaces` (still best-effort, no error variant). `is_git_repo` = `path/.git` exists.
The
fluorite-doc-comment-on-union-variant gotcha still applies; regenerate with
`cargo build -p models`.

### 5. Runtime scan — `runtime/src/scan.rs`

`exec(working_dir, req)` → `exec(registry: &WorkspaceRegistry, req) -> Vec<WorkspaceScan>`:
select the roots from `req.workspace` (`None` → all in registry order; `Some(name)` →
the one the registry maps that name to, or none if unknown), then for each `(name,
path)` run the existing single-root logic (instruction precedence + skills glob) plus an
`is_git_repo` check. This registry lookup is the **single name→path translation site**,
shared with `dispatch` (§9). `runtime/src/main.rs`'s `ScanWorkspace` arm sends the
`Vec` in `ScanResponse.workspaces`.

### 6. Client interpretation — `workflow/src/workspace.rs`

`WorkspaceContext` becomes a list; skills are grouped **per workspace** (the existing
`SkillSet`/`Skill`/frontmatter parsing is reused unchanged, applied per workspace):

```rust
pub struct WorkspaceContext { pub workspaces: Vec<WorkspaceInfo> }
pub struct WorkspaceInfo {
    pub name: String,
    pub path: String,
    pub is_git_repo: bool,
    pub instructions: Option<String>,
    pub skills: Arc<SkillSet>,
}
```

`scan(client, workspace: Option<String>)` issues the scan with that filter and
interprets `Vec<WorkspaceScan>` → `Vec<WorkspaceInfo>` (the spawn path passes `None`).
Within a workspace, duplicate skill names are kept-first with a warning (as today);
**across** workspaces there is no dedup — `october/git-bisect` and `shared/git-bisect`
coexist. `RuntimeClient::scan_workspace` gains the matching `workspace` argument.

### 7. Prompt composition

A single `# Workspaces` section, one labeled block per workspace; empty subsections
omitted. The header tells the agent that fs/exec and skill tools take a `workspace`:

```
# Workspaces
Filesystem, bash, and skill tools take a `workspace` argument naming one of these.

## october — /abs/october (git)
<AGENTS.md contents>
### Skills (load with the skill tool: workspace="october")
- git-bisect: Find the bad commit

## shared — /abs/shared
### Skills (load with the skill tool: workspace="shared")
- codegen: Regenerate protocol models
```

When there's exactly one workspace, the block still renders (named) but the header
notes `workspace` is optional. `compose_system_prompt` keeps role-first ordering and
returns `None` only when there is genuinely nothing to emit.

### 8. Skill + inspect tools — `workflow/src/context.rs`

Both tools re-scan live (unchanged mechanism) and are workspace-aware. The caller
passes a workspace **name**, which both tools forward **straight to the runtime scan's
`workspace` filter (§4)** — so name→path translation happens at the runtime, against
the same `WorkspaceRegistry` the fs/exec tools use (§9), never in the client. The
client only *interprets* the returned bytes (frontmatter → body / catalog), which is
content parsing, not translation.

The two bits of UX that need the full name set — the "optional iff single" default and
"unknown workspace, valid names are …" errors — use the **workspace name list captured
from the spawn-time scan** and handed to the toolbox at construction (`for_agent`).
Names are stable for the job (unlike skills), so this needs no extra round-trip and
still keeps the client out of path resolution.

- `skill`: input `{ workspace?: string, name: string }`. `workspace` is **optional iff
  exactly one workspace exists** (defaults to it; determined from the cached name list);
  with multiple, omitting it → `InvalidInput` listing the names. Forward the resolved
  `workspace` to `scan(client, Some(name))`, then `name` → body from the returned files.
  Unknown workspace (empty scan) / unknown skill → `InvalidInput` naming valid options.
- `inspect_workspace` (**replaces `list_skills`**): input `{ workspace?: string }`.
  Re-scans and returns the **current full catalog**: for the named workspace (or all,
  when omitted), its `path`, `is_git_repo`, whether an instruction file is present, and
  its skills listing (name + description). It returns **metadata only, never bodies** —
  use `skill` for a skill body and `read_file` for an updated instruction file, keeping
  the result compact and consistent with progressive disclosure. No "what to reload"
  selector: the scan is a single sweep that always returns everything, so a selector
  would only trim output for no real gain (YAGNI).

> Naming note: the tool is `inspect_workspace`, deliberately *not* `scan_workspace` —
> `ScanWorkspace` is the internal wire op (§4), and an agent-facing tool sharing that
> name would conflate "the tool the model calls" with "the protocol message."

### 9. Filesystem/exec tools — `fluorite/runtime.fl` + `runtime/src/tools/*`

Add `workspace: Option<String>` to `BashInput`, `ReadFileInput`, `EditFileInput`,
`WriteFileInput` (every tool with a path/cwd). `runtime::tools::dispatch` takes the
`WorkspaceRegistry` instead of a single `working_dir` and resolves the field:

- Present → look up the root; absent + single workspace → that root; **absent +
  multiple → the runtime returns a `ToolError`** ("specify a workspace: <names>") —
  the model never silently lands in an unintended root. Unknown name → `ToolError`
  listing valid names. This is enforced in `dispatch`, i.e. runtime-side, since that
  is where the I/O (and the registry) lives.
- `bash` sets `current_dir` to the resolved root; `read/edit/write` join `path` onto it.
- **No path-jailing.** A `path` is `join`ed onto the resolved root as today; `..` or an
  absolute path is not policed. There is no need: the sandbox is the security boundary,
  and every workspace in the job is one the user deliberately added and the agent
  already has full authority over — so traversing between roots is not an escalation
  (the agent could just name the other workspace). Jailing would guard a non-problem.

The tool input schemas advertised to the model gain the `workspace` property with a
description pointing at the `# Workspaces` list.

## Failure handling & resume

- **Scan transport failure**: warn + empty `WorkspaceContext` (no workspaces), exactly
  as the single-root path degrades today; the run proceeds.
- **A single workspace's files missing**: that workspace's block is sparse (no
  instructions / no skills); other workspaces unaffected.
- **Tool omits `workspace` with >1 workspace**: `ToolError` returned to the model
  (recoverable — it retries with a name), never a process failure.
- **Resume**: `JobSpec.workspaces` is persisted, so names and roots are stable across
  resume; the first spawn after resume re-scans all roots and picks up edits. Nothing
  about the scan is journaled.

## Testing

- `models`: `derive_workspaces` — basenames, single-segment conflict, multi-segment
  walk-up, identical-path error, order preservation.
- `runtime/src/scan.rs`: `workspace: None` → one entry per registry workspace;
  `Some(known)` → just that root; `Some(unknown)` → empty; correct `is_git_repo`,
  per-root instruction precedence + skills glob, missing files → sparse entry (tempdirs).
- `runtime/src/sandbox.rs` (or a focused unit): `WorkingDir` grant expands to every
  root (assert `allow_path` called per root).
- `runtime/src/tools/*`: `workspace` resolution — named, single-default, missing-with-
  multiple → error; bash cwd + read/write land in the right root.
- `workflow/src/workspace.rs`: interpret `Vec<WorkspaceScan>` → grouped skills (no
  cross-workspace dedup; intra-workspace kept-first); `compose_system_prompt` renders
  one block per workspace, omits empty subsections, single-workspace note.
- `workflow/src/context.rs`: `skill` with/without `workspace`; ambiguous (multi,
  omitted) → `InvalidInput`; unknown workspace and unknown skill cases.
  `inspect_workspace` with/without `workspace` returns the per-workspace catalog
  (path, git, instruction-presence, skills) and never bodies.
- **e2e** (`cli`/`workflow` `tests/`): run a workflow against **two** workspaces, each
  with an `AGENTS.md` + a skill; assert the prompt has both blocks, `skill(workspace,
  name)` returns the right body per workspace, and a `bash`/`write_file` targeting each
  workspace lands in the correct root.

## Out of scope (YAGNI)

- **Distributed/relay scan** (`executor-client/src/ws_transport.rs`): still no scan
  command; multi-workspace must eventually thread the registry through it, tracked as
  the existing follow-up — not addressed here.
- Per-agent workspace **scoping** (explorer-reads-all / worker-writes-one): the
  data model (named workspaces) makes this a later layer; v1 gives every agent all
  workspaces.
- Manifest file / explicit `name=path` override / git-remote-derived names.
- Path-jailing tool inputs to their workspace subtree.
- User/global (`~/.claude/skills`) and nested `CLAUDE.md` merging (already out of scope
  upstream).

## Touched files (summary)

- `models/src/lib.rs` — `Workspace`, `derive_workspaces`, `WorkspaceError`.
- `fluorite/daemon.fl` — `SubmitRequest.workdir` → `workdirs: Vec<String>`.
- `fluorite/runtime.fl` — `ScanRequest.workspace` filter; per-workspace `WorkspaceScan`
  (+`name`/`path`/`is_git_repo`), `ScanResponse.workspaces`, `workspace: Option<String>`
  on the fs/exec tool inputs.
- `cli/src/main.rs`, `cli/src/daemon/mod.rs` — comma-split `--workdir`; derive names.
- `supervisor/src/spec.rs` — `JobSpec.workspaces: Vec<Workspace>` (+ `RuntimeConfig`).
- `executor/src/process_provider.rs` — repeatable `--workspace name=path`.
- `runtime/src/main.rs` — parse `--workspace`, build `WorkspaceRegistry`, thread it.
- `runtime/src/sandbox.rs` — `apply` over a slice of roots.
- `runtime/src/scan.rs` — scan per workspace, `is_git_repo`.
- `runtime/src/tools/{dispatch,bash,read_file,edit_file,write_file}.rs` — resolve
  `workspace` → root.
- `workflow/src/workspace.rs` — `WorkspaceContext`/`WorkspaceInfo`, per-workspace
  interpret + prompt blocks.
- `runtime-client` — `scan_workspace` (trait + client + `MockTransport`) gains the
  `workspace: Option<String>` filter argument.
- `workflow/src/context.rs` — `for_agent` gains the spawn-scan workspace name list;
  workspace-aware `skill` (forwards the filter to the runtime scan); rename
  `list_skills` → `inspect_workspace` returning the full per-workspace catalog
  (`LIST_SKILLS_TOOL` const → `INSPECT_WORKSPACE_TOOL = "inspect_workspace"`).
- `models` — regenerated from the schema.
