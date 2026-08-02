# Drop the `workspace` parameter from the path-taking tools

Closes [#94](https://github.com/blossomstack/horsie/issues/94).

## Problem

`workspace` is a second addressing scheme layered on top of absolute paths, and it
carries information in only one of the three jobs it does.

`WorkspaceRegistry::resolve` (`runtime/src/workspace.rs:66`) maps the field to a root:
`Some("horsie_shared")` → the plugin library; `Some(name)` → that workspace; `None` +
exactly one workspace → that one; `None` + several → an error listing the names.

Every path-taking tool then does `working_dir.join(input.path)`, and `Path::join`
discards the base when the argument is absolute. The prompt already hands the model
each workspace's absolute path (`## api — /ws/api (git)`), so `read_file("/ws/web/foo")`
works today with no `workspace` argument. For a normal workspace the parameter is
redundant.

It is also actively harmful. #86 added a sticky per-agent cwd and immediately collided
with it: an explicit `workspace` was silently ignored whenever a cwd override was set,
so a call naming workspace B read — and `write_file` would have written — inside
workspace A. That was fixed by giving an explicit `workspace` precedence (`base()` in
`runtime/src/tools/mod.rs:93`), which is a precedence rule standing in for one mechanism
too many.

The one place a name genuinely is the only handle is the shared plugin library. It is
held outside the workspace list (`WorkspaceRegistry` keeps `workspaces: Vec<Workspace>`
and `plugins_dir: Option<PathBuf>` as separate fields), never appears in the
`# Workspaces` block, and its absolute path is never sent to the model. A shared skill's
sibling files are reachable only via `read_file(workspace="horsie_shared", ...)`.

## Approach

Remove the parameter and give the model the paths it was standing in for.

### 1. Tools lose the parameter

`bash`, `read_file`, `write_file`, `find_and_replace`, `replace_lines`, `list_files`,
`glob`, `grep`, and `set_working_dir` drop `workspace` from their input schemas and
from the wire structs in `models/fluorite/runtime.fl`.

A call's base directory becomes: the agent's cwd override if set, else the **first**
workspace in registry order. Registry order is preserved end to end (`derive_workspaces`
→ `WorkspaceDef` → `--workspace` flags), so `workspaces[0]` is a stable primary. No
workspaces configured stays an error.

`resolve(&Option<String>)` collapses to a no-argument `default_root() -> Result<PathBuf,
String>`; the `Some(name)` branch and the `multiple workspaces; specify one of:` error
disappear. `base()` collapses to `state.effective_dir(agent, &registry.default_root()?)`
— with one addressing mechanism left there is nothing for a precedence rule to arbitrate.

`set_working_dir` is included even though #94 lists only the eight path tools. Its `set`
arm used `workspace` as a base, which an absolute path expresses; its `reset` arm only
validated the name and otherwise ignored it. Leaving it would keep the precedence rule
alive for exactly one caller.

### 2. Tools that keep the parameter

`skill` and `inspect_workspace` select *among* sources rather than resolving a base, and
`ScanRequest.workspace` is an internal RPC field no model ever sees.

For `skill` the name is scoped to a source, unlike a path, which is globally unique: two
workspaces can each define `deploy`, and a workspace skill can shadow a plugin one.
Keeping the parameter means loading a skill can never silently return the wrong version.

### 3. The model gets the paths

`ScanResponse` gains the shared library's absolute root so it can be named in the prompt.

The `# Workspaces` intro stops advertising an argument that no longer exists and states
where the working directory starts:

> Your working directory starts at /ws/api. Filesystem and bash tools resolve relative
> paths against it; use an absolute path to reach another workspace, or set_working_dir
> to move.

The shared section header gains its root, and is described as shared rather than
read-only — read-only is not enforced anywhere (the sandbox's `WorkingDir` grant covers
workspace roots only, `runtime/src/main.rs:146`).

Each skill line gains its directory, relative to the section root already in the header:

```
## api — /ws/api (git)
### Skills (load with the skill tool, workspace="api")
- deploy — .claude/skills/deploy/: Ship a build to staging or production

# Shared skills — /home/u/.local/share/horsie/plugins
Shared across all workspaces. Load with the skill tool, workspace="horsie_shared".
- brainstorming — superpowers/skills/brainstorming/: Turn an idea into a design
```

Relative rather than absolute because the root is one line above and the shared prefix
would otherwise repeat on every line of a ~20-skill plugin library.

### 4. The resources footer

Loading a skill appends a `[resources]` footer naming its directory. Today only shared
skills get one, and it is phrased in terms of `workspace="horsie_shared"`. It becomes an
absolute path, and workspace skills get one too:

```
[resources] This skill's files are in /ws/api/.claude/skills/deploy/.
Read one with read_file(path="/ws/api/.claude/skills/deploy/<file>").
```

The runtime already sends each workspace skill's absolute path in
`WorkspaceScan.skills[].path`; `parse_skill` discards it (`rel_dir: None`). This is the
one piece beyond #94's literal scope. It costs nothing at prompt time — the footer
appears only when a skill is loaded — and it removes the asymmetry that made
`horsie_shared` special.

## Compatibility

Wire-compatible in both directions. Fluorite-generated structs carry no
`deny_unknown_fields`, and `Option` fields are `skip_serializing_if = "Option::is_none"`
and default to `None` when absent. An old server sending `workspace` to a new runtime
has it ignored; a new server omitting it against an old runtime resolves as `None`.

The one skew that misbehaves is a new server against an old runtime in a multi-workspace
session, which hits the old `multiple workspaces; specify one of:` error. Server and
runtime ship together.

The sandbox is unaffected: grants are per workspace root and absolute-path addressing
was already reachable.

## Surface

- `models/fluorite/runtime.fl` — drop `workspace` from nine input structs; add the
  shared root to `ScanResponse`.
- `runtime/src/workspace.rs` — `resolve` → `default_root`; `select` and
  `SHARED_WORKSPACE` unchanged.
- `runtime/src/tools/mod.rs` — delete `workspace_of` and the `base` precedence rule.
- `runtime/src/tools/set_working_dir.rs` — drop the parameter from both arms.
- `runtime/src/scan.rs` — return the plugins root.
- `runtime-client/src/tools/` — delete `with_workspace` and `workspace_arg`; update the
  nine tool specs.
- `runtime-client/src/{transport,client}.rs` — plumb the shared root.
- `workflow/src/workspace.rs` — skill directories relative to their section root; new
  `# Workspaces` intro; shared header with root.
- `workflow/src/context.rs` — absolute `[resources]` footer for both kinds of skill.

## Testing

- `default_root` returns the first workspace with several configured, and errors with
  none.
- A tool call with a cwd override set resolves against the override; without one, against
  `workspaces[0]`.
- No tool schema contains a `workspace` property except `skill` and `inspect_workspace`.
- The composed prompt renders each skill's directory relative to its section root, and
  the shared header carries the plugins root.
- Loading a workspace skill and a shared skill each append a footer with the correct
  absolute directory.
- Existing runtime, workflow, and e2e suites updated for the removed field.
