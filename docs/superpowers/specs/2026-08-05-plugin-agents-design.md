# Plugin agents

#105's Phase 2. A plugin ships `agents/*.md`; each is a named subagent with its own system
prompt, tool allowlist and model. horsie can already spawn subagents — what it cannot do is
spawn *a named kind of* subagent.

## What the issue said, and what is actually left

#105 scoped Phase 2 as four items, of which two claimed there was no consumer for a plugin
agent at all: "sub-agents exist only in the `workflow` crate's agent graphs", "there is no
dispatch tool in the session path". Both were true when it was written and are not now.
`spawn_agent` / `subagent_status`, the persisted subagent tree with depth and concurrency
limits, per-subagent transcripts and `ContentPart::SubAgentResult` all shipped in #183–#190.

So the work is discovery and selection, not dispatch:

1. find and parse `agents/*.md`, and the manifest `agents` field that may relocate them
2. carry the catalogue out of the runtime, which is the only process that can see plugin files
3. let a spawn name one, and offer the model the list
4. honour `model`, `tools` and `effort` when the subagent runs

## What the ecosystem actually declares

Measured across the official marketplace: **8 plugins ship `agents/`, 31 agent files between
them.** Frontmatter keys, by how many files use them:

| key | files | horsie |
| --- | --- | --- |
| `name` | 31 | required |
| `description` | 31 | required — it is what the model picks on |
| `model` | 23 | honoured when it names a model horsie has |
| `tools` | 22 | mapped to horsie's `allowed_tools` |
| `color` | 19 | **not read** — nothing renders it |
| `effort` | 7 | mapped to `thinking_effort` |
| `initialPrompt` | 1 | **not read** |

**Every declared `model` is an alias, never a model id**: `inherit` (11), `sonnet` (8),
`opus` (4). That decides the model question below. Zero manifests declare an `agents` field,
so the conventional `agents/` directory is the only shape in the wild — the manifest field is
supported because the spec has it and because impeccable-shaped relocations are exactly what
Phase 0 existed to fix.

## Where each piece lives

**Split along the line skills already use**: the runtime finds files and ships their bytes,
the server interprets them. The runtime is the only process where plugin files exist; parsing
frontmatter is not file access.

- `horsie_support::plugin::agents` — `agent_files(plugin_root, manifest)`. The manifest
  `agents` field is a string or array, exactly like `skills`, and each entry may name a
  directory (its `*.md`) or a single file. Absent, it is `agents/`.
- `runtime/src/plugins.rs::discover_agents` — mirrors `discover_skills`, emitting
  `PluginAgent { plugin, rel_path, content }` into `ScanResponse.shared_agents`, gated on
  `include_shared` like the skills it sits beside.
- `workflow/src/workspace.rs` — parses frontmatter into `PluginAgentDef`, reusing
  `split_frontmatter`, and hangs the set off `SharedContext` beside `skills`.

### `tools`, and the inverse of the alias table

A declared allowlist names Claude's tools (`Glob, Grep, LS, Read, …`). horsie's are
snake_case. This is the same mismatch `claude_aliases` exists to solve for hook matchers, run
backwards, so it is the same table read the other way — `horsie_tools_for("Read") == ["read_file"]`
— rather than a second table that can drift from the first. A pinned test asserts the two are
inverses.

Names with no horsie equivalent (`TodoWrite`, `WebFetch`, `KillShell`, …) map to nothing,
which is the honest answer: horsie cannot offer them. An allowlist that maps to nothing at all
leaves a subagent with no tools, which is allowed — it can still answer from its prompt — and
logged.

`allowed_tools` is an existing `AgentSettings` field, so honouring it is passing a different
value, not new machinery.

### `model`

Honoured **only when it names a model in horsie's catalogue**, else inherited, with the
declared value logged. Given the measurement above this means every plugin agent in the wild
inherits — which is what `inherit`, the plurality, asks for anyway.

The alternative is mapping `sonnet` / `opus` onto whatever horsie's catalogue happens to hold.
That is a guess, and it is wrong in the case that matters: a session pointed at kimi or
deepseek would silently switch provider mid-session because a plugin author wrote `sonnet` on
a file. A plugin does not get to choose the user's provider.

## Selecting one

`spawn_agent` gains an optional `agent_type`. The catalogue goes in the tool's *description*
rather than as a JSON `enum`, because a name alone does not tell the model when to pick it —
the `description` frontmatter is the whole point of the field, and every one of the 31 files
sets it. With no agents installed the parameter is omitted from the schema entirely, so a
session with no plugins sees exactly the tool it sees today.

`SubAgentToolbox` holds the catalogue (it is built in `provide()`, where the scan is) and
rejects an unknown name locally, naming the ones that exist. The session actor never learns
what an agent type is — it journals the string.

**`agent_type` is journaled on `SubAgentRecord`** and resolved to a definition at the
subagent's own `provide()`, not carried in memory from the spawn. Two reasons: the record
already survives offload and recovery, and the definition is a property of the plugin library
as it is *now* — an agent whose plugin was removed between spawn and wake must fail loudly
rather than run with a prompt nobody can point at.

The subagent's system prompt becomes the agent's body in place of `SUBAGENT_PROMPT_SUFFIX`;
the workspace and skill sections around it are unchanged, because a named agent still works in
the same workspace with the same skills.

## Two things that fall out

**`SubagentStart` / `SubagentStop` get a real matcher domain.** `SessionContextProvider::
agent_type()` returns the constant `"subagent"` today, with a comment naming this phase as
what would fix it. It now returns the spawned agent's type, so a hook may match `reviewer`
and fire for reviewers only. Untyped spawns keep `"subagent"`.

**`PluginRoot::is_installable` widens.** It is skills-only, and its own doc comment says
widening it "to hooks/agents/commands is Phase 1 of #105 — and this is the single place it
changes". That did not happen in Phase 1, so a hooks-only plugin is refused at install today
even though its hooks would run. A plugin is installable when it provides *anything* horsie
runs: skills, hooks, or agents. The rejection message names all three.

## Testing

- support: manifest `agents` as string, array, directory and single file; the conventional
  default; the alias table's two directions are inverses.
- workflow: frontmatter with every measured key; a file missing `name`/`description` is
  skipped rather than half-parsed; `tools` maps through the alias table; an unknown `model` is
  dropped rather than passed on.
- server: spawning with a known type applies its prompt and allowlist; an unknown type is a
  tool error naming what exists; the type reaches `SubagentStart`'s matcher; a record written
  before typed agents existed still spawns.
- The `agent_type` parameter is absent from the tool schema when no agents are installed.

## Not in scope

`color` and `initialPrompt` (no consumer). Agents declared by the *workspace* rather than a
plugin (`.claude/agents/`) — a plugin catalogue is what #105 asks for, and the workspace case
is a strictly larger scan surface with no measured demand. Slash commands are Phase 3.
