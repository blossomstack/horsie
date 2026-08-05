# Slash commands

#105's Phase 3. A plugin ships `commands/*.md`; each is a named prompt template the user
invokes as `/name args`. horsie has no slash-command concept anywhere today — not in the
session server, not in any client.

Split in two: this design is the server (parse, catalogue, expansion, hook), and the web
picker follows on top of it. The split is along the API: once `GET /sessions/:id/commands`
exists and `/name args` expands, the picker is a composer affordance with nothing new
underneath it.

## What the ecosystem declares

Measured across the official marketplace: **13 plugins ship `commands/`, 29 files, none in
subdirectories.**

| frontmatter key | files | horsie |
| --- | --- | --- |
| `description` | 29 | required — the picker and the catalogue both read it |
| `argument-hint` | 18 | shown in the picker |
| `allowed-tools` | 14 | narrows the turn's toolbox |
| `disable-model-invocation` | 2 | **not read** — horsie never offers commands *to the model* |
| `hide-from-slash-command-tool` | 2 | **not read**, same reason |

There is no `name` field: **a command's name is its filename**. Body substitutions:
`$ARGUMENTS` in 11 files, `$1..$N` in 11, and `` !`bash` `` in **2**.

The two unread keys are both about a "slash command tool" horsie does not have — commands are
a *user* affordance here, never something the model invokes. Reading them would imply a
capability that does not exist.

## Where expansion happens

**On the pre-run seam**, the one #215 built: it is the only place that has the prompt *and* a
runtime, which is exactly what `UserPromptSubmit` needed and what `` !`bash` `` needs too. It
also puts expansion where the spec puts it — `UserPromptExpansion` fires there, before
`UserPromptSubmit` sees the result.

Server-side rather than client-side, so the CLI and the API get commands for free and
`` !`bash` `` runs in the sandbox rather than wherever the client happens to be.

**The catalogue is scanned only when the prompt starts with `/`.** The seam runs before
`provide()`, so it has no scan of its own, and scanning every turn to serve a feature used on
a minority of them is the wrong trade. A prompt that cannot be a command costs nothing.

### The expanded message

Expansion produces one framed message, the same shape hook-injected context already uses:

```
<command name="review" args="src/foo.rs">
…the command's body, substituted…
</command>
```

One message, so nothing about journaling or `AgentInput` changes. The model reads the body;
the frame tells a client it is looking at an invocation rather than at something the person
typed by hand, which is what lets the transcript render `/review src/foo.rs` and keep the body
collapsed.

The alternative — journal the raw `/review …` and send the expansion — needs `AgentInput` to
carry display text distinct from wire text, which is a journal-shaped change for a rendering
problem. Rejected.

### Substitution

- `$ARGUMENTS` — everything after the command name, verbatim.
- `$1 … $9` — positional, shell-split on whitespace with quotes honoured. An unset position
  substitutes empty, which is what a template with an optional tail expects.
- `` !`cmd` `` — run in the sandbox via the runtime's bash tool, output substituted. **Gated
  on `allowed-tools` naming Bash**: a command that did not ask for a shell does not get one
  through the back door, and a template that interpolates without declaring it is a template
  whose author expected the declaration to matter. Failure substitutes the error text rather
  than failing the turn — the model can see what went wrong.
- `@path` is **not** substituted. It appears in 2 files and means "read this file" in a client
  that eagerly attaches files; horsie's agent has `read_file` and does not need the prompt
  pre-loaded with content it may not want.

`allowed-tools` narrows the turn's toolbox for the turn the command starts, through the same
`allowed_tools` field a session already has and the same Claude→horsie alias table plugin
agents use.

## `UserPromptExpansion`

Currently classified `NoConcept` — "no slash commands". It becomes describable *and* wired,
firing on the seam immediately before expansion, with the raw prompt as its payload. It may
inject context and it may block (which refuses the prompt, as `UserPromptSubmit`'s block
does). That takes horsie to eight wired events.

## Shape

- `horsie_support::plugin::commands` — `command_files()` (manifest `commands` field, else
  `commands/`) and `parse(name, content) -> PluginCommandDef`. Name from the filename;
  `description` required.
- `horsie_support::plugin::commands::expand` — the pure substitution, taking the already-run
  `` !`cmd` `` outputs as a map so the engine itself does no I/O and is testable without a
  sandbox.
- Runtime `discover_commands` → `ScanResponse.shared_commands`, gated on `include_shared`
  exactly as skills and agents are.
- `workflow::CommandCatalog` beside `AgentCatalog`, on `SharedContext`.
- `SessionContextProvider::start_hooks` gains the expansion step and returns
  `TurnPreparation { records, message }` rather than a bare `Vec<HookRecord>` — the seam
  already carries the turn's message through `PreparedStart`, so a rewritten one has somewhere
  to go without new machinery.
**`GET /api/sessions/:id/commands` is deferred to the picker**, not built here. The
catalogue comes from the library scan, which happens in `provide()` — so an endpoint has two
honest implementations: answer from the last turn's scan, which is empty on a session's first
message and so useless exactly when a picker is most wanted; or scan on demand, which is
correct but costs a runtime round-trip per read. Choosing between those is a decision the
picker's own behaviour should drive, and until a picker exists the endpoint has no caller.
Typing `/name` works from every client without it.

## Testing

- support: name from filename; a file with no `description` is skipped; `$ARGUMENTS`,
  `$1..$9`, quoted arguments, an unset position, and a `` !`cmd` `` both allowed and refused.
- runtime: commands are discovered and gated with skills and agents.
- server: `/review foo` expands and frames; an unknown `/name` is left alone (it may simply be
  a message beginning with a slash); a prompt not starting with `/` never scans;
  `allowed-tools` narrows the turn; `UserPromptExpansion` fires before `UserPromptSubmit` and
  can refuse.

## Not in scope

The web picker (its own change). Commands declared by the *workspace* rather than a plugin.
`disable-model-invocation` and `hide-from-slash-command-tool`, which describe a
model-facing slash-command tool horsie does not have.
