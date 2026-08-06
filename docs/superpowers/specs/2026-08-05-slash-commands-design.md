# Slash commands, from a catalogue the server already owns

#105's Phase 3. A plugin ships `commands/*.md`; each is a named prompt template the user
invokes as `/name args`. horsie has no slash-command concept anywhere today — not in the
session server, not in any client.

This replaces an earlier version of this design that discovered commands by scanning the
*runtime* on every prompt beginning with `/`. That was the wrong place to look: the server
installs plugin bundles itself, so it already knows what every bundle offers, and asking a
sandbox on the far side of a socket to re-derive it cost a full workspace scan per prompt to
answer a question the server could answer from its own database.

## What the ecosystem declares

Measured across the official marketplace: **13 plugins ship `commands/`, 29 files, none in
subdirectories.**

| frontmatter key | files | horsie |
| --- | --- | --- |
| `description` | 29 | required — the picker and the catalogue both read it |
| `argument-hint` | 18 | shown in the picker |
| `allowed-tools` | 14 | **not read** — see below |
| `disable-model-invocation` | 2 | **not read** — horsie never offers commands *to the model* |
| `hide-from-slash-command-tool` | 2 | **not read**, same reason |

There is no `name` field: **a command's name is its filename**. Body substitutions:
`$ARGUMENTS` in 11 files, `$1..$N` in 11, and `` !`bash` `` in **2**.

## The catalogue

**The server's plugin library is the single source of truth for what a bundle offers.**
Installing a bundle already clones it, runs `PluginRoot::inspect` on the checkout, and packs
the subtree into a content-addressed zip on the server's disk; the `plugins` row already
stores facts derived from that inspection. Commands, skills and agents join them.

`pack()` gains a catalogue, persisted as JSON in a new nullable `catalog` column.
`0025_plugin_catalog.sql`, in both dialects, adds it and drops `skill_count` in the same
migration — the count is derivable from the catalogue, and two sources for one fact is how
they drift:

```
[{ kind: "command" | "skill" | "agent",
   name, description,
   argumentHint?,        // commands only
   template? }]          // commands only — the body the server expands
```

Each kind is read with the parser that already exists for it, all under
`horsie_support::plugin`: `commands::parse` over `PluginRoot::command_files`, the `skills`
reader over `skill_dirs`, the `agents` reader over `agent_files` — the three lists `inspect`
already returns. A file with no `description` is skipped — every one of the 29 published
commands sets it, and an entry a picker cannot label cannot be offered.

Rows installed before this ships have `catalog = NULL`. The artifact zip is still on disk, so
`PluginService` parses and persists it on first read. Self-healing rather than a migration
that cannot run: SQL cannot open a zip, and "your commands vanished until you re-install" is
not a defensible upgrade.

### Why not the runtime

The two scans are provably identical, which is what makes deleting one safe rather than
merely convenient. `pack()` inspects `plugin_root` and zips *that same subtree* minus `.git`,
which `inspect` never reads; `materialize` unpacks the verified zip into
`<plugins_dir>/<bundle_name>`; `discover_*` inspects that directory. Same function, same
bytes, same result — including the bundle name, since the runtime's fallback is the directory
name the server chose.

Where they differ, the server is the one a picker must believe:

- **A bundle that failed to land.** `provision_plugins` is best-effort by design, so a
  download failure leaves the runtime with fewer bundles than were selected. Expanding from
  the database means `/commit` still works.
- **A picker with no session yet.** The new-session screen has no runtime to ask, and that is
  exactly where completions matter most.

The runtime keeps scanning skills and agents. That scan feeds the *model* — the system
prompt's skill list and `spawn_agent`'s catalogue — and "what actually landed on disk" is the
right answer to that question. The catalogue answers a different one: what the user selected.
Two readers of the same bundles, deliberately, because they are asking different things.

## The API

**No new endpoint.** `GET /api/plugins` already returns `PluginView[]`, already requires no
session and no runtime, and is already what the settings page and the new-session picker
fetch. It gains the catalogue.

`PluginView` carries every entry's `kind`, `name`, `description` and `argumentHint` — and
**not** `template`. The client never expands, so shipping template bodies (`code-review.md`
runs past a page) to every client on every plugins list buys nothing.

`skill_count` is deleted rather than kept beside the catalogue it is now derivable from. One
source of truth, and the row that displayed it is being reworked anyway.

## Expansion

Expansion runs on the pre-run seam #215 built — the one place with the prompt in hand before
the turn starts. What changed is where it *looks*: a database read of the session's selected
bundles, not a scan across the wire.

`SessionContextProvider` gains `plugins: Vec<String>` (from `ActorSpec::plugins`) and a handle
to `ServerDeps::plugins`, whose trait widens by one method:

```rust
async fn catalog(&self, names: &[String]) -> Vec<CatalogEntry>;
```

Empty `names` resolves to the `enabled_default` set, mirroring provisioning. `CatalogEntry`
here is the *stored* shape — templates included — which is what separates it from the
template-free `PluginView` the clients receive.

**Recognising an invocation.** One parser, parameterised by sigil, so `/name` and `@name`
cannot disagree: leading position only, a name of alphanumerics plus `-` and `_`, everything
after it arguments. An unknown name is passed through exactly as typed — `/etc/hosts` and an
email address both have to survive, so an unrecognised name is never an error.

**Substitution**, commands only:

- `$ARGUMENTS` — everything after the name, verbatim.
- `$1 … $9` — positional, shell-split with quotes honoured. An unset position substitutes
  empty, which is what a template with an optional tail expects.
- `@path` is **not** substituted: it means "eagerly attach this file" in a client that does
  that, and horsie's agent has `read_file`.

**The framed message.** One shape per kind:

```
<command name="review" args="src/foo.rs">…substituted template…</command>
<skill   name="brainstorming" args="a new API">…instruction naming that skill…</skill>
<agent   name="code-reviewer" args="check this">…instruction to delegate…</agent>
```

Skills and agents carry no template, so their body is a fixed instruction naming the entry;
the agent then reaches for the skill tool or `spawn_agent` as it already would. One message,
so nothing about journaling or `AgentInput` changes.

`name` and `args` are XML-escaped, and newlines in `args` collapse to spaces **in the
attribute only** — the body still receives the real multi-line arguments. The attribute
exists so a renderer can show `/review src/foo.rs` and keep the body collapsed, and an
argument containing a quote would otherwise break the frame that renderer parses.

The seam's return widens from `Vec<HookRecord>` to `TurnPreparation { records, message }`.
`PreparedStart` already carried the turn's message, so a rewritten one has somewhere to go.

Rejected: journalling the raw `/review …` and sending the expansion. That needs `AgentInput`
to carry display text distinct from wire text — a journal-shaped change for a rendering
problem.

### `` !`cmd` `` is not supported

Two of 29 published commands interpolate a shell, both gathering `git status`-shaped context
for a commit. A template can simply instruct the agent to run those commands; it has bash.

Supporting it costs the whole path: a gate on `allowed-tools`, a tool call per snippet in
front of a waiting user, stderr and exit-code handling, and an interaction with `PreToolUse`.
Templates using it still expand — the snippets substitute empty.

With the snippets gone, `allowed-tools` has no consumer, so it is not read. Narrowing a
turn's toolbox from a command is a real feature and a separate decision: a command that
silently removes tools mid-session is a surprise, not a convenience.

### `UserPromptExpansion`

Currently classified `NoConcept` — "no slash commands". It becomes describable *and* wired,
firing on the seam immediately before expansion with `{prompt, kind, name}` and the name as
its matcher domain, so a hook can guard `/deploy` rather than every prompt. It may inject
context and it may block, which refuses the prompt as `UserPromptSubmit`'s block does. That
takes horsie to eight wired events.

A refusal is read with `start_blocked`, not `halt_reason`. The two are not the same statement:
`halt_reason` sees only `continue: false`, so a hook answering `{"decision":"block"}` would
otherwise be noticed a layer later — after expansion had already happened.

## UI

**Skills settings.** `BundleRow`'s single derived line becomes a per-kind summary with zero
kinds omitted — `3 skills · 2 commands · 1 agent` — over a disclosure listing the entries **as
they are typed**:

```
/commit          Create a git commit
/code-review     Code review a pull request
@code-reviewer   Reviews a diff for correctness
```

The settings page becomes the reference for what a bundle actually gives you, which it cannot
be today.

**Composer typeahead.** `/` or `@` **at the start of the message** opens a filtered list —
start-of-input only, matching the parser exactly, so the menu and the server never disagree
about what counts as an invocation. Arrows move, Tab or Enter selects, Escape closes.
Selecting inserts `/name ` and leaves the cursor after it, with `argument-hint` as placeholder
text.

Enter is the hazard: with the menu open it selects, with it closed it sends. Getting that
wrong sends a half-written message, so it is a test rather than a detail.

A `useEntryCatalog` hook derives the effective list from the existing `usePlugins()` query,
filtered by the session's selected bundle names and falling back to the `enabled_default` set
when a session selected none — the same rule the server uses. A presentational `EntryMenu`
renders it. `Composer` gains trigger detection and wiring only; it is 142 lines and says in
its own doc comment that this is deliberate.

Because the source is `GET /api/plugins`, completions work on the **new-session screen**:
check and uncheck bundles and the list changes, with no session and no runtime.

## Failure

Every failure resolves to "behave as if the feature is not there".

- Catalogue absent or unparseable → that bundle contributes no entries.
- Unknown `/name` → passed through as typed.
- A name declared by two bundles → first alphabetically wins, the loser is logged. The rule
  skills and agents already use.
- No plugin service wired (tests, minimal deploys) → empty catalogue, no expansion, no error.

## Testing

- **support**: name from filename; a file with no `description` is skipped; `$ARGUMENTS`,
  `$1..$9`, quoted arguments, an unset position; XML escaping of a quoted argument; both
  sigils, and the non-invocations `/`, `/etc/hosts` and prose containing a slash.
- **ingest**: a fixture bundle of all three kinds produces the catalogue; `update` re-derives
  it; a NULL column is backfilled on read; `PluginView` omits templates.
- **server**: each kind expands and frames; an unknown name is left alone; a blocking
  `UserPromptExpansion` hook stops the expansion; no runtime call is made for any of it.
- **web**: Vitest for `useEntryCatalog` (including the empty-selection fallback) and for
  `EntryMenu`'s Enter split; one Playwright test typing `/` and picking.

## Not in scope

Narrowing a turn's toolbox from `allowed-tools`. Commands declared by the *workspace* rather
than a plugin. `disable-model-invocation` and `hide-from-slash-command-tool`, which describe a
model-facing slash-command tool horsie does not have. `@name` spawning a real subagent rather
than instructing the main agent to delegate — a new execution path, not a message rewrite.
