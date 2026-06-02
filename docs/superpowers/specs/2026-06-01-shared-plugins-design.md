# Shared plugins: install once, skills + bootstrap hooks for every job

- **Date:** 2026-06-01
- **Status:** design, pending implementation
- **Scope:** add a **machine-global, read-only plugin library** (e.g. install
  [obra/superpowers](https://github.com/obra/superpowers) once) whose skills and
  `SessionStart` hooks are available to opted-in agents across *every* job, in addition
  to the per-workspace `.claude/skills` already supported. Builds directly on
  [2026-05-31-workspace-context-loading-design.md](2026-05-31-workspace-context-loading-design.md),
  [2026-05-31-live-skill-loading-design.md](2026-05-31-live-skill-loading-design.md),
  and [2026-06-01-multi-workspace-design.md](2026-06-01-multi-workspace-design.md).

## Goal

A user runs once:

```bash
horsie plugin install https://github.com/obra/superpowers
```

and from then on every opted-in agent in every job sees superpowers' skills in its
prompt, can load any skill's body and read that skill's sibling `references/`/`scripts/`
files, and — crucially — has superpowers' `using-superpowers` discipline injected at
agent start via the plugin's `SessionStart` hook. Narrow tool-running agents can opt out
so the skill-discovery pressure never derails them.

## Motivation

Horsie **already implements the Agent Skills primitive**, scoped to workspaces:
`runtime/src/scan.rs` globs `.claude/skills/*/SKILL.md` per root; `workflow/src/workspace.rs`
parses `name`/`description` frontmatter, lists skills in the prompt; the `skill` tool
loads a body live (`workflow/src/context.rs`). What is missing for "install superpowers
for everything" is four things, none of which the skill primitive itself provides:

1. **A global skill source.** Skills are tied to a job's workspaces; there is no
   machine-level library shared across jobs.
2. **Manifest awareness.** Plugins ship a `.claude-plugin/plugin.json` and bundle skills
   under `skills/<name>/SKILL.md`; horsie reads neither.
3. **Hooks.** superpowers only *works* because a `SessionStart` hook force-injects its
   bootstrap skill at conversation start. Horsie has no hook concept, so a naive
   "scan a global dir" yields skill *listings* but not the activation discipline that
   makes the library effective.
4. **Install/management tooling.** No way to fetch, list, update, or remove a library.

The ecosystem (anthropics/skills, obra/superpowers, alirezarezvani/claude-skills, the
ComposioHQ/travisvn awesome-skills catalogs) is uniform: `SKILL.md` + `name`/`description`
frontmatter + `.claude-plugin/`. A single design targeting that format generalizes. The
one design-affecting nuance (validated against **anthropics/skills**) is that a plugin can
**remap its skills location via the manifest** (`strict:false` + a `skills` array) rather
than always using `skills/`, so discovery must honor a manifest path override.

## Decisions

Settled during brainstorming; not open:

| # | Decision | Choice |
|---|----------|--------|
| 1 | Format support | **Skills + a `SessionStart` bootstrap hook.** Not bare skill-listing (would never auto-activate); not full agents/commands/MCP (those map awkwardly onto workflow-defined agents) |
| 2 | Install model | **Horsie-managed global dir.** `horsie plugin install <git-url>` clones into a managed dir; a lockfile records installs; jobs auto-load it. Not `~/.claude/plugins` reuse, not user-managed config paths |
| 3 | Hook fidelity | **Generic command hook.** Parse `hooks.json`, *run* the `SessionStart` command inside the existing sandboxed runtime, capture its output as injected context. Not static file injection |
| 4 | Agent scope | **Opt-in per agent, default-on at workflow level.** A `use_plugins` flag gates surfacing + injection; narrow subagents set it `false` |
| 5 | Skill load site | **In the sandboxed runtime** (same scan path as workspaces), *not* client-side in the daemon — so the agent's `read_file`/`bash` tools can reach a skill's sibling resources (gap #6) |
| 6 | Reserved source name | The plugin library is addressed as the reserved workspace-like token **`horsie_shared`** in the `skill`, `inspect_workspace`, and filesystem/exec tools |
| 7 | Sibling resources | **In scope for v1.** Loading a shared skill reports its directory under `horsie_shared`; the agent reads `references/`/runs `scripts/` via the normal fs/exec tools against that root |

**Why load in-sandbox (#5).** Decision #3 already forces the sandboxed runtime to know
the plugins dir and hold a read grant on it (the hook command executes in-sandbox with
`${CLAUDE_PLUGIN_ROOT}` pointed at the plugin). Given that cost is already paid, routing
skill text through the **same runtime scan path** keeps one code path *and* lets fs/exec
tools read sibling files (#7). Client-side loading would leave those siblings unreadable
and create a second skill code path.

**Why a reserved name, not a new tool (#6).** The multi-workspace work already gave the
`skill`/`inspect_workspace`/fs/exec tools a `workspace` argument with name→path resolution
in the runtime registry. Modelling the library as the reserved name `horsie_shared` reuses
all of that — no new tool, no schema fork. `horsie_shared` is *read-only*; the sandbox
grant (not the toolbox) enforces that, consistent with "the sandbox is the security
boundary" from the multi-workspace design.

**Why default-on (#4).** "Shared for all" is the intent. Default-on is harmless until a
plugin is installed (no plugins ⇒ no shared skills, no hooks, the flag is a no-op), so
existing workflows are unaffected until the user opts into the *machine* by installing
something. Per-agent `use_plugins: false` keeps focused subagents clean.

## Layout & install

### On-disk layout

```
<plugins_dir>/                         # default: <data_dir>/plugins, config: storage.plugins_dir
  plugins.json                         # lockfile (CLI-owned; for list/update UX)
  superpowers/                         # one dir per installed plugin (= horsie_shared root subtree)
    .claude-plugin/plugin.json         # manifest (optional per spec; name/version/skills override)
    skills/<name>/SKILL.md             # skills (location overridable by manifest)
    skills/<name>/references/*.md      # sibling resources, reachable via #7
    hooks/hooks.json                   # SessionStart hook
```

`plugins_dir` defaults to `<data_dir>/plugins` (durable installed assets live with the
durable data, beside the job journal) and is overridable via a new `storage.plugins_dir`
config field. `horsie_shared` resolves to `plugins_dir`; a skill's `rel_dir` is its skill
directory **relative to `plugins_dir`** (e.g. `superpowers/skills/brainstorming`).

### `horsie plugin` command group (`cli/`)

Host-side operations (the CLI runs `git` directly — **not** sandboxed; installs are a
deliberate, trusted user action):

- `horsie plugin install <git-url> [--name N] [--ref R]` — `git clone [--branch R]` into
  `<plugins_dir>/<name>/`; read `.claude-plugin/plugin.json` for the canonical name +
  version (fallback: repo basename / git sha); verify it exposes ≥1 skill; append to
  `plugins.json`. Re-installing an existing name errors unless `--force`.
- `horsie plugin list` — table from `plugins.json`: name, version, source, ref, #skills,
  #hooks.
- `horsie plugin update <name>` — `git pull` (or re-clone at `--ref`); refresh the
  lockfile entry's sha/version.
- `horsie plugin remove <name>` — delete the dir + lockfile entry.

Lockfile entry shape (hand-written serde, CLI-owned — not a wire type):

```jsonc
{ "plugins": [ {
  "name": "superpowers", "source": "https://github.com/obra/superpowers",
  "ref": "main", "version": "5.1.0", "sha": "1a2b3c…"
} ] }
```

V1 installs a **plugin repo directly by git URL** (superpowers' own repo is a valid
plugin at root: `.claude-plugin/plugin.json` + `skills/`). Marketplace resolution
(`marketplace.json` → choose plugin → clone its `source`) is a documented follow-up — a
thin indirection over the same clone, not needed for the headline use case.

## Architecture

### Data flow (new pieces in **bold**)

```
horsie plugin install …                → clone into <plugins_dir>/<name>, write plugins.json   [host]

horsie job run --workdir a --input "…"
  → CLI resolves capabilities, **adds Read DirGrant for <plugins_dir>** (if it has plugins)
       **+ resolves node (config runtime.hook_path else `which node`) → grant + hook PATH**
  → SubmitRequest { … }                                                   (daemon wire, unchanged)
  → JobSpec { …, **plugins_dir** }                                        (storage)
  → RuntimeConfig { workspaces, **plugins_dir** }                         (executor → runtime)
  → runtime --workspace name=path (×N) **--plugins-dir <path>**
       registry = user workspaces + **reserved horsie_shared → plugins_dir (read-only)**
  → per opted-in agent spawn:
       ScanWorkspace ⇒ Vec<WorkspaceScan> **+ shared_skills: Vec<PluginSkill>**   (one round-trip)
       **RunSessionStart ⇒ context**  (runs each plugin's SessionStart cmd in-sandbox)
       compose_system_prompt: **# Session bootstrap** + role + # Workspaces + **# Shared skills**
  → skill / inspect_workspace / read_file / bash accept **workspace="horsie_shared"**
```

### Effective opt-in

`effective_use_plugins(agent, workflow) = agent.use_plugins ?? workflow.default_use_plugins ?? true`.
Computed in `WorkflowActor::spawn_agent`. When `false`, the agent behaves exactly as
today: no shared-skills scan, no `# Shared skills` section, no `SessionStart` injection,
and the `skill`/`inspect_workspace` tools reject `horsie_shared`. (Fs/exec resolution of
`horsie_shared` stays available process-wide — read-only and harmless — rather than being
gated at two layers; the opt-in governs *visibility and injection*, the sandbox governs
*access*.)

### 1. Opt-in surface — `fluorite/workflow.fl`

```
struct WorkflowAgentDef {            // … existing fields …
    use_plugins: Option<bool>,       // per-agent override; None ⇒ inherit workflow default
}
struct WorkflowDefinition {
    start: String,
    agents: Vec<WorkflowAgentDef>,
    default_use_plugins: Option<bool>, // None ⇒ true (default-on)
}
```

Regenerate `models` (`cargo build -p models`); the fluorite-doc-comment-on-union-variant
gotcha noted in prior specs still applies to any new doc comments.

### 2. Config + storage — `cli/src/config.rs`, `supervisor/src/spec.rs`

- `StorageConfig.plugins_dir: PathBuf`, defaulting to `<data_dir>/plugins` (a `default_fn`
  that mirrors the data-dir resolution then joins `plugins`). An empty/absent plugins dir
  is legal (the whole feature degrades to "no shared skills").
- `JobSpec` gains `plugins_dir: PathBuf` (persisted, so a resumed job keeps the same
  library root). `RuntimeConfig` (executor.fl) gains `plugins_dir: String` and
  `hook_path: Vec<String>` (resolved interpreter dirs, §3a).
- `RuntimeConfig` (config.rs, the CLI-owned one) gains `hook_path: Option<Vec<PathBuf>>`
  — the interpreter override (§3a).

### 3. Capability grant — `cli/src/capabilities.rs`

When resolving the spec at submit time, if `plugins_dir` exists and contains ≥1 plugin,
append `Grant::Dir(DirGrant { path: plugins_dir, access: Read })`. **Read-only**: write
tools targeting `horsie_shared` fail closed at the sandbox. No `CapabilitySpec` schema
change — these are extra `DirGrant`s. For each resolved interpreter dir (§3a) also append
a Read `DirGrant` so the sandbox can read+exec it (e.g. node and its sibling libs); the
default capability spec already permits process exec generally (the `bash` tool relies on
it), so the missing piece is path *reachability*, which the grant supplies.

### 3a. Interpreter resolution for hooks — `cli/src/config.rs`, `cli/src/`

A plugin's hook command may shell out to an interpreter (superpowers' `run-hook.cmd`
invokes `node`). horsie does not invoke it directly, so it must be reachable on the hook's
PATH *and* readable by the sandbox. Resolution happens **CLI-side at submit time** — the
CLI runs in the user's interactive shell, so its `PATH` already reflects the active
version-managed node (nvm/fnm/volta), which a daemon's stripped env would not:

1. **Override:** `runtime.hook_path: Option<Vec<PathBuf>>` in config — explicit
   directories prepended to the hook PATH. Setting it to your node `bin` dir makes
   superpowers work deterministically.
2. **Default (auto-discover):** when unset, resolve `node` against the CLI's ambient
   `PATH` (`which`/`command -v`); if found, use its directory. ("Default to the node in
   the environment.")
3. **Neither:** `hook_path` stays empty. Hooks that need a missing interpreter fail
   gracefully (non-fatal — bootstrap omitted, a warning tells the user to set
   `runtime.hook_path`).

The resolved dirs travel to the runtime via `RuntimeConfig.hook_path` (executor.fl) and
drive both the sandbox grants (§3) and the hook PATH (§6). The mechanism is interpreter-
agnostic — `node` is only the *default probe*; a python/deno/bun hook is served by setting
`hook_path` (or just having it on the ambient PATH at submit).

### 4. Runtime registry — `runtime/src/main.rs`, `runtime/src/workspace.rs`

- New arg `--plugins-dir <path>` (single, optional).
- The `WorkspaceRegistry` gains a **reserved** entry `horsie_shared → plugins_dir`, flagged
  read-only and **excluded from the "default when exactly one workspace" count** so it
  never becomes the implicit target and never changes single-workspace ergonomics. A user
  workspace deriving to the name `horsie_shared` is rejected at submit (reserved-name
  validation in `derive_workspaces`).
- fs/exec tools (`dispatch`) resolve `horsie_shared` like any other name → `plugins_dir`;
  reads succeed, writes are blocked by the sandbox. This is what delivers #7 for free.

### 5. Plugin discovery + skill scan — `runtime/src/plugins.rs` (new), `runtime/src/scan.rs`

A pure-ish module that, given `plugins_dir`, enumerates each subdir that is a plugin
(`.claude-plugin/plugin.json` present, **or** a bare `skills/` dir / root `SKILL.md`):

1. **Resolve the skills location.** Manifest `skills` field (string *or* array of paths,
   relative to the plugin root) if present; else default `skills/`. Glob `<loc>/*/SKILL.md`
   per location. (Honoring the override is what makes anthropics/skills-style repos work.)
2. **Emit `PluginSkill` per SKILL.md** carrying the plugin name, `rel_dir` (skill dir
   relative to `plugins_dir`), and raw `content`. Frontmatter parsing is reused from
   `workflow/src/workspace.rs` (`name`/`description`), unchanged.

The scan is folded into the existing `ScanWorkspace` round-trip so spawn still does one
request. `ScanRequest` gains `include_shared: bool` (set by the client only for opted-in
agents); `ScanResponse` gains `shared_skills: Vec<PluginSkill>`:

```
struct ScanRequest {                 // … existing …
    include_shared: Bool,            // false ⇒ runtime skips plugin enumeration
}
struct PluginSkill { plugin: String, rel_dir: String, content: String }
struct ScanResponse { call_id: String, workspaces: Vec<WorkspaceScan>, shared_skills: Vec<PluginSkill> }
```

`include_shared=false` ⇒ empty `shared_skills` (no plugin I/O). Best-effort throughout: an
unreadable plugin or malformed manifest is skipped with a warning, never failing the scan.

### 6. SessionStart hooks — `runtime/src/plugins.rs`, `fluorite/runtime.fl`

New op `RunSessionStart`, because hooks must *execute commands in the sandbox*:

```
struct SessionStartRequest  { call_id: String }
struct SessionStartResponse { call_id: String, context: String }
// added to RuntimeInboundMessage / RuntimeOutboundMessage unions
```

For each plugin (in stable dir order) the runtime reads `hooks/hooks.json` (or a manifest
`hooks` override), collects `SessionStart` entries' `command` strings, substitutes
`${CLAUDE_PLUGIN_ROOT}` → the plugin dir, and runs each with:

- **cwd** = the plugin dir,
- **env** `CLAUDE_PLUGIN_ROOT` = plugin dir, and `PATH` = `RuntimeConfig.hook_path` dirs
  (§3a) prepended to the inherited `PATH`, so the plugin's script finds `node`,
- **stdin** = a minimal Claude-Code SessionStart payload (`{"hook_event_name":"SessionStart","source":"startup"}`),
- a bounded timeout, output clamped (reuse the bash tool's clamping).

Output handling: if stdout parses as the CC hook envelope
(`{"hookSpecificOutput":{"additionalContext":"…"}}`) use `additionalContext`; otherwise
use raw stdout. Concatenate all plugins' contexts with separators → `context`. A
non-zero-exit / timed-out hook is logged and skipped (its context omitted), never fatal.

The runtime owns hook discovery + execution + env so the semantics live in one place and
run behind the sandbox. (v1 runs hooks **per opted-in agent spawn**; superpowers' bootstrap
is idempotent text. Caching one run per job is a noted optimization, not v1.)

### 7. Client interpretation — `workflow/src/workspace.rs`

- `scan(client, workspace, include_shared)` forwards `include_shared`. Returned
  `shared_skills` are parsed (reusing `parse_skill`) into a `SharedSkillSet` keyed by
  skill name; cross-plugin name collisions are **kept-first with a warning** (mirrors the
  intra-workspace rule). Each retained skill keeps its `plugin` + `rel_dir`.
- A `SharedContext { skills: SharedSkillSet, bootstrap: Option<String> }` is assembled by
  `spawn_agent` for opted-in agents (`bootstrap` = the `RunSessionStart` context, `None`
  if empty) and threaded into prompt composition.

`Skill` gains an optional `rel_dir: Option<String>` (None for workspace skills, Some for
shared) so the `skill` tool can emit the resource hint (#7).

### 8. Prompt composition — `workflow/src/workspace.rs`

`compose_system_prompt(agent_prompt, ws, shared: Option<&SharedContext>)`:

```
# Session bootstrap                      ← shared.bootstrap, prepended verbatim (opted-in only)
<plugin SessionStart output, e.g. the using-superpowers discipline>

<role / agent_prompt>

# Workspaces                             ← unchanged
…

# Shared skills (load with the skill tool, workspace="horsie_shared")   ← opted-in only
- brainstorming: Use this before any creative work …
- test-driven-development: Use when implementing any feature …
```

`# Session bootstrap` leads (it is framing/meta, matching CC's "context at session
start"). Both shared sections are omitted when empty or when the agent is opted out, so an
opted-out agent's prompt is byte-identical to today's.

### 9. `skill` / `inspect_workspace` — `workflow/src/context.rs`

The toolbox learns the agent's `use_plugins` and the reserved name:

- **`skill(name, workspace?)`** — `workspace="horsie_shared"` loads a shared skill:
  re-scan with `include_shared=true`, return the body **plus a resource hint** when the
  skill has sibling files, e.g.:
  > `This skill's files live under workspace "horsie_shared" at superpowers/skills/brainstorming/. Read a resource with read_file(workspace="horsie_shared", path="superpowers/skills/brainstorming/references/x.md").`

  This hint is what operationalizes #7 — the agent already has `read_file`/`bash` against
  `horsie_shared`. For an opted-out agent, `horsie_shared` → `InvalidInput`
  ("plugins are not enabled for this agent").
- **`inspect_workspace(workspace?)`** — includes a `horsie_shared` section (path =
  plugins_dir, skills = the shared catalog) for opted-in agents; omitted otherwise.
- `horsie_shared` is **not** counted by the "optional iff single workspace" rule; it is
  always addressed explicitly by name. The reserved name is added to the toolbox's valid-name
  set (for opted-in agents) so error messages list it.

## Failure handling & resume

- **No plugins installed / empty plugins dir**: `include_shared` yields nothing, no grant
  is added, hooks produce no context — the feature is fully inert; behavior == today.
- **Scan transport failure** (relay mode has no scan yet): degrades to empty context with
  a warning, as today; shared skills simply absent.
- **Malformed manifest / unreadable plugin / bad SKILL.md**: skipped with a warning;
  other plugins unaffected.
- **Hook non-zero exit / timeout / missing interpreter**: logged, that plugin's bootstrap
  omitted; the run proceeds (skills still listed). Never fatal.
- **Reserved-name collision** (`horsie_shared` user workspace): rejected at submit.
- **Resume**: `JobSpec.plugins_dir` is persisted, so the library root is stable; the first
  spawn after resume re-scans live and re-runs hooks, picking up `plugin update`s. Nothing
  about the scan or hooks is journaled.

## Testing

- `cli` plugin commands: `install` clones (use a local file:// git repo fixture), parses
  manifest name/version, writes the lockfile, rejects re-install without `--force`;
  `list`/`update`/`remove` round-trip the lockfile; missing-manifest fallback to dir name.
- `cli/src/config.rs`: `plugins_dir` default = `<data_dir>/plugins`; override parses;
  `hook_path` override parses.
- interpreter resolution: explicit `hook_path` wins; unset → discovered from a stubbed
  `PATH` containing a fake `node`; absent node → empty `hook_path` (no panic).
- `cli/src/capabilities.rs`: a populated plugins dir adds exactly one Read `DirGrant`;
  each resolved interpreter dir adds one; an empty/absent dir + no interpreter add none.
- `runtime/src/plugins.rs`: skills discovery with default `skills/`, with a manifest
  `skills` string override, with an array override, with no manifest (dir-name plugin);
  `rel_dir` correctness; malformed manifest skipped. Hook collection: `SessionStart`
  command extraction, `${CLAUDE_PLUGIN_ROOT}` substitution, raw-stdout vs
  `additionalContext` envelope, non-zero exit → omitted (use a trivial `echo`/`exit 1`
  fixture, no node dependency in tests).
- `runtime/src/scan.rs`: `include_shared=false` ⇒ empty `shared_skills` and no plugin I/O;
  `true` ⇒ skills from a tempdir plugin tree.
- `runtime/src/workspace.rs` (registry): `horsie_shared` resolves to plugins_dir, is
  read-only, and is excluded from the single-default count; reserved-name validation.
- `workflow/src/workspace.rs`: parse `shared_skills` → `SharedSkillSet` (cross-plugin
  kept-first); `compose_system_prompt` emits `# Session bootstrap` + `# Shared skills` only
  when opted-in and non-empty; opted-out prompt unchanged.
- `workflow/src/context.rs`: `skill(workspace="horsie_shared")` returns body + resource
  hint; opted-out → `InvalidInput`; `inspect_workspace` shows/omits the `horsie_shared`
  section by opt-in; `effective_use_plugins` precedence (agent ?? workflow ?? true).
- **e2e** (`cli`/`workflow` `tests/`): install a fixture plugin (one skill with a
  `references/note.md`, one `SessionStart` hook that echoes a sentinel); run a two-agent
  workflow where agent A is opted-in and agent B sets `use_plugins:false`; assert A's
  prompt contains the bootstrap sentinel + the shared skill, A can `skill(...)` then
  `read_file(workspace="horsie_shared", …)` the sibling, and B's prompt has neither.

## Out of scope (YAGNI / follow-ups)

- **Marketplace resolution** (`marketplace.json` → pick plugin → clone `source`,
  github/url/git-subdir/npm source kinds). v1 = direct git-URL clone of a plugin repo.
- **Plugin components beyond skills + SessionStart hooks**: bundled agents, commands, MCP
  servers, output styles, other hook events (PreToolUse, etc.). The opt-in field and hook
  op leave room to add these later.
- **Per-agent plugin allowlist** (vs the boolean): a `use_plugins: ["superpowers"]` form is
  a later refinement; v1 is all-installed-or-none.
- **Splitting skill-surfacing from hook-injection** under separate flags.
- **Per-job hook caching** (run SessionStart once per job, not per spawn).
- **Richer frontmatter** (`allowed-tools`, `metadata`, multi-line descriptions): the parser
  still reads only flat `name`/`description`; extra fields are ignored.
- **Distributed/relay scan + hooks**: relay mode still has no scan command (existing gap);
  this widens but does not address it.

## Risks

- **Hook interpreter availability.** superpowers' `SessionStart` shells out to a
  `node`-backed `.cmd`. Addressed by §3a (config `runtime.hook_path` override, else
  auto-discover node from the CLI's ambient PATH) driving both the hook PATH and a sandbox
  read+exec grant. Residual risk: macOS seatbelt may still block exec of a node that links
  dylibs outside the granted dir — needs an implementation spike against the real
  superpowers plugin on macOS before calling this done. The fallback (hook fails ⇒ skills
  still listed, bootstrap omitted, warning emitted) keeps it non-fatal meanwhile.
- **Trust.** `plugin install` clones and later *executes* third-party hook code (in the
  sandbox). Installation is the trust boundary — documented as a deliberate user action,
  like installing any dependency.

## Touched files (summary)

- `fluorite/workflow.fl` — `WorkflowAgentDef.use_plugins`, `WorkflowDefinition.default_use_plugins`.
- `fluorite/runtime.fl` — `ScanRequest.include_shared`; `PluginSkill`; `ScanResponse.shared_skills`;
  `SessionStartRequest`/`SessionStartResponse` + the two union arms.
- `fluorite/executor.fl` — `RuntimeConfig.plugins_dir`, `RuntimeConfig.hook_path`.
- `cli/` — new `plugin` command group (`install`/`list`/`update`/`remove`) + lockfile module;
  node/interpreter resolution (config override else `which node`) at submit.
- `cli/src/config.rs` — `StorageConfig.plugins_dir` (+ default); `RuntimeConfig.hook_path`.
- `cli/src/capabilities.rs` — Read `DirGrant` for a populated plugins dir + each resolved
  interpreter dir.
- `cli/src/daemon/mod.rs`, `supervisor/src/spec.rs` — thread `plugins_dir` into `JobSpec`.
- `executor/src/process_provider.rs` — pass `--plugins-dir` and `--hook-path`.
- `runtime/src/main.rs`, `runtime/src/workspace.rs` — `--plugins-dir`; reserved read-only
  `horsie_shared` registry entry + reserved-name validation.
- `runtime/src/plugins.rs` (new) — plugin enumeration, manifest parse, skills discovery,
  `SessionStart` collection + execution.
- `runtime/src/scan.rs` — honor `include_shared`, return `shared_skills`.
- `runtime-client` — `scan_workspace` gains `include_shared`; new `run_session_start`
  (trait + client + `MockTransport`).
- `workflow/src/workspace.rs` — `SharedSkillSet`/`SharedContext`, `Skill.rel_dir`,
  `compose_system_prompt` bootstrap + shared-skills sections.
- `workflow/src/workflow_actor.rs` — `effective_use_plugins`; scan with `include_shared`;
  call `run_session_start`; thread `SharedContext` into compose + toolbox.
- `workflow/src/context.rs` — toolbox learns `use_plugins` + `horsie_shared`; `skill`
  resource hint; `inspect_workspace` shared section.
- `models` — regenerated from the schemas.
