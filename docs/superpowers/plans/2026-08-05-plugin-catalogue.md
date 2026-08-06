# Plugin catalogue and slash commands — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the server's plugin library the single source of truth for the commands, skills and agents a bundle offers, expose it on `GET /api/plugins`, expand `/name` and `@name` from it on the pre-run seam, and surface it in the skills page and the composer.

**Architecture:** `pack()` already inspects every bundle it installs; it now also parses each `commands/*.md`, `skills/*/SKILL.md` and `agents/*.md` into a catalogue stored as JSON on the `plugins` row. `PluginView` carries that catalogue minus template bodies, so both the settings page and the composer read it from a query they already make. The session seam expands an invocation by reading the same column — a database read, no runtime round-trip — which lets the runtime-side command scan added by PR #220 be deleted outright.

**Tech Stack:** Rust (axum, sqlx over `sqlx::Any`, tokio), fluorite schema codegen, React + TypeScript + Vitest + Playwright, bun for the web client.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-05-slash-commands-design.md`. Every task's requirements implicitly include it.
- Worktree: `.claude/worktrees/slash-catalog`, branch `feat/slash-catalog`. All commands run from that directory.
- Editing a `.fl` file regenerates the Rust types automatically via `models/build.rs`. TypeScript is **not** automatic and there are **two** trees: `clients/ts` and `clients/web`. Both must be regenerated or the web build breaks; CI only drift-checks `clients/ts`.
- The web client installs with **bun**, never npm: `cd clients/web && bun install`.
- Rust iteration uses `cargo test -p <crate> --lib`. Run the full workspace suite **once** before pushing, never twice in one command.
- Migrations are per-dialect: every schema change needs a file in **both** `server/migrations/sqlite/` and `server/migrations/postgres/`.
- Database writes go through `Db::begin_write()`, never `pool().begin()`.
- Commit messages: short subject, no body unless the diff hides context. Never list Claude as author or co-author.

---

### Task 1: Rebase onto main and delete the runtime-side command scan

The branch is behind `main` and does not compile: `test_runtime_manager` lost its second parameter. The runtime-side command discovery goes in the same task because both are "make the tree build against today's `main`", and a reviewer would reject or accept them together.

**Files:**
- Modify: `server/src/sessions/session_actor.rs` (two `test_runtime_manager` call sites)
- Modify: `models/fluorite/runtime.fl` — remove `PluginCommand`, `ScanResponse.shared_commands`
- Modify: `runtime/src/plugins.rs` — remove `discover_commands`
- Modify: `runtime/src/scan.rs` — remove `shared_commands`
- Modify: `runtime/src/main.rs` — remove the field it populates
- Modify: `workflow/src/workspace.rs` — remove `CommandCatalog`, `CatalogCommand`, `SharedContext.commands`, `SharedScan.commands`, the `interpret_shared` command arm and parameter
- Modify: `workflow/src/lib.rs` — drop the two re-exports
- Modify: `server/src/runtime_vendor/fake.rs` — drop the `shared_commands` builder and field
- Modify: `models/src/lib.rs`, `runtime-client/src/testkit.rs`, `runtime-vendor/src/socket_transport.rs` — drop the struct-literal fields
- Delete: `clients/ts/src/generated/runtime/pluginCommand.ts`, `clients/web/src/generated/runtime/pluginCommand.ts`

**Interfaces:**
- Consumes: nothing.
- Produces: a tree that builds against `main`. `SharedContext { skills, agents, root }` returns to its pre-PR shape. `horsie_support::plugin::commands` survives untouched.

- [ ] **Step 1: Rebase**

```bash
git fetch origin main
git rebase origin/main
```

- [ ] **Step 2: Fix the two broken call sites**

`test_runtime_manager` now takes one argument. In `server/src/sessions/session_actor.rs`, both occurrences of `test_runtime_manager(&vendors, tmp.path())` become `test_runtime_manager(&vendors)`.

- [ ] **Step 3: Delete the runtime-side command discovery**

Remove every item listed in **Files** above. `interpret_shared` loses its `raw_commands` parameter, so its four test call sites lose their extra `Vec::new()`.

- [ ] **Step 4: Regenerate both TypeScript trees**

```bash
make ts-types
cd clients/web && bun install && bun run generate-types && cd ../..
git status --short clients/
```

Expected: `pluginCommand.ts` deleted in both trees, `scanResponse.ts` and `index.ts` updated in both.

- [ ] **Step 5: Verify the workspace builds and tests pass**

```bash
cargo test -p horsie-workflow --lib -p horsie-runtime --lib
cargo build --workspace
```

Expected: PASS, no reference to `shared_commands` anywhere (`grep -rn shared_commands --include='*.rs' --include='*.fl' --include='*.ts' .` returns nothing).

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "commands: drop the runtime-side scan; the server owns the catalogue"
```

---

### Task 2: Catalogue types and parsers in `horsie_support`

The pure layer: what an entry is, how each kind is parsed, how an invocation is recognised, how a frame is built. No I/O, no database, so it is testable on its own.

**Files:**
- Create: `support/src/plugin/catalog.rs`
- Modify: `support/src/plugin/mod.rs` — `pub mod catalog;`
- Modify: `support/src/plugin/skills.rs` — add `parse`
- Modify: `support/src/plugin/commands.rs` — generalise `parse_invocation`, escape `frame`, delete the shell and `allowed-tools` support

**Interfaces:**
- Consumes: `horsie_support::plugin::{commands, agents, skills}`, `PluginRoot`.
- Produces:
  - `catalog::CatalogKind` — `Command | Skill | Agent`, serialized lowercase.
  - `catalog::CatalogEntry { kind, name, description, argument_hint: Option<String>, template: Option<String> }`, `Serialize + Deserialize + Clone + Debug + PartialEq`.
  - `catalog::build(root: &PluginRoot) -> Vec<CatalogEntry>` — sorted by `(kind, name)`.
  - `catalog::frame(kind, name, args, body) -> String`.
  - `skills::parse(content: &str) -> Option<(String, String)>` returning `(name, description)`.
  - `commands::parse_invocation(prompt: &str, sigil: char) -> Option<(&str, &str)>`.
  - `commands::expand(template: &str, args: &str) -> String`.

- [ ] **Step 1: Write the failing tests for the pure layer**

In `support/src/plugin/catalog.rs`, a `mod tests` covering: a fixture plugin directory with one command, one skill and one agent produces three entries sorted by kind then name; a command with no `description` is skipped; a skill with no `description` is skipped; `frame` escapes `&`, `<`, `>` and `"` in `name` and `args` and collapses a newline in `args` to a space while leaving the body alone.

In `support/src/plugin/commands.rs`, extend `an_invocation_is_a_leading_slash_name` to cover both sigils: `parse_invocation("/review x", '/')` is `Some(("review", "x"))`, `parse_invocation("@bot x", '@')` is `Some(("bot", "x"))`, and each returns `None` for the other's sigil.

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test -p horsie-support --lib plugin::catalog
```

Expected: FAIL — `catalog` module does not exist.

- [ ] **Step 3: Implement**

`skills::parse` mirrors `parse_skill` in `workflow/src/workspace.rs:402` — split frontmatter, read `name` and `description`, both required — so the server and the runtime agree on what a skill is.

`catalog::build` walks `root.command_files` through `commands::parse` (name from the file stem), `root.skill_dirs` reading each `SKILL.md` through `skills::parse`, and `root.agent_files` through `agents::parse` (which reads its own `name` from frontmatter). Anything that fails to parse is skipped, not fatal.

`commands::parse_invocation` gains a `sigil: char` parameter; the body is otherwise unchanged.

`commands::frame` moves to `catalog::frame`, gaining the kind as the element name and XML-escaping `name` and `args`:

```rust
fn attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}
```

Delete from `commands.rs`: `shell_snippets`, the `shell` parameter of `expand`, `allowed_tools` on `PluginCommandDef` and its frontmatter arm, and the tests covering them.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p horsie-support --lib
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add support/
git commit -m "support: a plugin catalogue of commands, skills and agents"
```

---

### Task 3: Ingest builds the catalogue

**Files:**
- Modify: `server/src/plugins/ingest.rs` — `PluginBundle.catalog`, built in `pack()`
- Test: the existing `mod tests` in the same file

**Interfaces:**
- Consumes: `catalog::build`.
- Produces: `PluginBundle.catalog: Vec<CatalogEntry>`.

- [ ] **Step 1: Write the failing test**

`a_bundle_catalogues_its_commands_skills_and_agents` — build a fixture plugin with `commands/commit.md` (`description: Create a git commit`), `skills/tdd/SKILL.md` (`name: tdd`, `description: d`) and `agents/reviewer.md` (`name: reviewer`, `description: d`), ingest it, and assert the catalogue has exactly those three entries with the right kinds and names.

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p horsie-server --lib plugins::ingest
```

Expected: FAIL — no `catalog` field.

- [ ] **Step 3: Implement**

In `pack()`, after `let root = PluginRoot::inspect(plugin_root)?;`, add `let catalog = horsie_support::plugin::catalog::build(&root);` and carry it on `PluginBundle`. `skill_count` is removed from `PluginBundle` in Task 4, not here.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p horsie-server --lib plugins::ingest
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server/src/plugins/ingest.rs
git commit -m "plugins: catalogue a bundle's entries at ingest"
```

---

### Task 4: Persist the catalogue, drop `skill_count`

**Files:**
- Create: `server/migrations/sqlite/0025_plugin_catalog.sql`
- Create: `server/migrations/postgres/0025_plugin_catalog.sql`
- Modify: `server/src/plugins/store.rs` — `COLS`, `PluginRow`, `row_to_plugin`, `upsert`
- Modify: `server/src/plugins/ingest.rs` — drop `skill_count` from `PluginBundle`
- Modify: `server/src/plugins/service.rs` — the `PluginRow` construction sites

**Interfaces:**
- Consumes: `CatalogEntry`.
- Produces: `PluginRow.catalog: Vec<CatalogEntry>`; `PluginRow.skill_count` no longer exists.

- [ ] **Step 1: Write the migrations**

Both dialects:

```sql
ALTER TABLE plugins ADD COLUMN catalog TEXT;
ALTER TABLE plugins DROP COLUMN skill_count;
```

- [ ] **Step 2: Write the failing test**

In `server/src/plugins/store.rs` tests: `a_catalogue_round_trips` — upsert a row with two entries, read it back with `get`, assert the entries survive; and a row whose `catalog` column is NULL reads back as an empty vec rather than failing.

- [ ] **Step 3: Run it to verify it fails**

```bash
cargo test -p horsie-server --lib plugins::store
```

Expected: FAIL — no `catalog` field.

- [ ] **Step 4: Implement**

Add `catalog` to `COLS`, to `PluginRow`, to the `INSERT`'s column list, its `VALUES` placeholders and its `ON CONFLICT` assignments, and bind `serde_json::to_string(&row.catalog)`. In `row_to_plugin`, read `Option<String>` and `serde_json::from_str` it, defaulting to `Vec::new()` on NULL or a parse error — a corrupt column must degrade to "no entries", never fail a list. Remove `skill_count` everywhere it appears in this crate.

- [ ] **Step 5: Run the tests**

```bash
cargo test -p horsie-server --lib plugins
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add server/
git commit -m "plugins: persist the catalogue, drop the derived skill count"
```

---

### Task 5: Serve the catalogue on `PluginView`, backfill lazily

**Files:**
- Modify: `models/fluorite/plugins.fl` — `CatalogEntryView`, `PluginView.catalog`, remove `skill_count`
- Modify: `server/src/plugins/service.rs` — `row_to_view`, lazy backfill in `list`
- Regenerate: both TypeScript trees

**Interfaces:**
- Consumes: `PluginRow.catalog`.
- Produces: `PluginView.catalog: Vec<CatalogEntryView>` where `CatalogEntryView { kind, name, description, argument_hint }` — **no template**.

- [ ] **Step 1: Write the failing tests**

`a_view_carries_the_catalogue_without_templates` — install a fixture bundle with a command whose template is `Review $1`, list, and assert the view's entry has the name and description but that the serialized JSON does not contain `Review $1`.

`a_null_catalogue_is_backfilled_from_the_artifact` — install a bundle, blank its `catalog` column directly, list, and assert the entries come back and the column is no longer null.

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test -p horsie-server --lib plugins::service
```

Expected: FAIL.

- [ ] **Step 3: Implement the wire types**

In `models/fluorite/plugins.fl`, add above `PluginView`:

```
/// One entry a bundle offers, as the picker and the settings page show it.
/// No template: the server expands, so a client never needs the body.
struct CatalogEntryView {
    /// "command", "skill" or "agent".
    kind: String,
    name: String,
    description: String,
    /// `argument-hint`, shown beside the name. Commands only.
    argument_hint: Option<String>,
}
```

Replace `skill_count: u32` on `PluginView` with `catalog: Vec<CatalogEntryView>`.

- [ ] **Step 4: Implement the service**

`row_to_view` maps each `CatalogEntry` to a `CatalogEntryView`, dropping `template`. `list` collects rows whose catalogue is empty, re-derives each from `ArtifactStore::path(&row.artifact_hash)` by unzipping to a temp dir and calling `catalog::build`, persists via `upsert`, and uses the result. A bundle whose artifact is missing stays empty and is logged — never an error.

- [ ] **Step 5: Regenerate both TypeScript trees and run the tests**

```bash
make ts-types
cd clients/web && bun run generate-types && cd ../..
cargo test -p horsie-server --lib plugins
```

Expected: PASS; `catalogEntryView.ts` exists in both trees.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "plugins: serve the catalogue on the bundle view"
```

---

### Task 6: Expand from the catalogue on the seam

**Files:**
- Modify: `server/src/plugins/mod.rs` — `PluginProvisioner::catalog`
- Modify: `server/src/plugins/service.rs` — implement it
- Modify: `server/src/sessions/session_actor.rs` — provider fields, `expand_command`, the refusal check
- Modify: `models/fluorite/runtime.fl` — `UserPromptExpansionInput` gains `kind`
- Modify: `support/src/plugin/hooks/invoke.rs` — carry `kind`

**Interfaces:**
- Consumes: `catalog::{CatalogEntry, CatalogKind, frame}`, `commands::{parse_invocation, expand}`.
- Produces: expansion with no runtime call.

- [ ] **Step 1: Write the failing tests**

In `session_actor.rs` tests, replacing the PR's `command_harness` (which scripted a fake runtime scan) with one that seeds the plugin service:

- `a_slash_command_expands_into_its_framed_template`
- `a_skill_and_an_agent_expand_under_their_own_sigils` — `/tdd` frames as `<skill>`, `@reviewer` as `<agent>`
- `an_unknown_command_leaves_the_prompt_alone` — `/nosuch`, `/etc/hosts`, plain text
- `a_blocking_expansion_hook_stops_the_expansion` — a hook returning `decision: block` must leave the message unexpanded **and** abandon the turn (this is the bug in the current PR)
- `expansion_makes_no_runtime_scan` — assert the fake runtime recorded no scan call

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test -p horsie-server --lib sessions::session_actor::tests
```

Expected: FAIL.

- [ ] **Step 3: Implement**

`PluginProvisioner` gains:

```rust
async fn catalog(&self, names: &[String]) -> Vec<CatalogEntry>;
```

resolving empty `names` to `default_names()`, reading each bundle's row, and concatenating — first alphabetically wins on a name collision, with the loser logged.

`SessionContextProvider` gains `plugins: Vec<String>` (from `spec.plugins`) and `plugin_catalog: Option<Arc<dyn PluginProvisioner>>` (from `deps.plugins`), set at both construction sites (`session_actor.rs:682` and `:761`).

`expand_command` becomes: try `parse_invocation(prompt, '/')` then `('@')`; return `None` if neither matches or `use_plugins()` is false; read the catalogue; find the entry, requiring the sigil to match the kind (`/` for command and skill, `@` for agent); fire `UserPromptExpansion`; **return early if `start_blocked(&records).is_some()`**; build the body (`commands::expand` for a command, a fixed instruction for a skill or agent); return `catalog::frame(...)`.

Delete `COMMAND_SHELL_TIMEOUT_SECS`, the `may_shell` block and the `client.invoke` call.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p horsie-server --lib sessions
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "commands: expand from the catalogue, not from a runtime scan"
```

---

### Task 7: Skills page shows what a bundle offers

**Files:**
- Modify: `clients/web/src/pages/settings/skills/BundleRow.tsx`
- Test: `clients/web/src/pages/settings/skills/BundleRow.test.tsx` (create)

**Interfaces:**
- Consumes: `PluginView.catalog`.
- Produces: nothing other tasks depend on.

- [ ] **Step 1: Write the failing test**

`renders_per_kind_counts_and_expands_to_entries` — render a `PluginView` with two commands, one skill and one agent; assert the summary reads `1 skill · 2 commands · 1 agent`, that entries are hidden initially, and that clicking the disclosure reveals `/commit` and `@reviewer`.

- [ ] **Step 2: Run it to verify it fails**

```bash
cd clients/web && bun run test BundleRow
```

Expected: FAIL.

- [ ] **Step 3: Implement**

Replace the `{bundle.skillCount} skill…` line with counts derived from `bundle.catalog`, omitting zero kinds, over a `<button>`-toggled list rendering each entry as its trigger plus name plus description. Commands and skills show `/`, agents `@`.

- [ ] **Step 4: Run the test**

```bash
cd clients/web && bun run test BundleRow
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/pages/settings/skills/
git commit -m "web: show a bundle's commands, skills and agents"
```

---

### Task 8: Composer typeahead

**Files:**
- Create: `clients/web/src/hooks/useEntryCatalog.ts`
- Create: `clients/web/src/components/EntryMenu.tsx`
- Modify: `clients/web/src/components/Composer.tsx`
- Test: `clients/web/src/hooks/useEntryCatalog.test.ts`, `clients/web/src/components/EntryMenu.test.tsx`

**Interfaces:**
- Consumes: `usePlugins()`, `PluginView.catalog`.
- Produces:
  - `useEntryCatalog(selected: string[] | undefined): CatalogEntryView[]`
  - `<EntryMenu entries sigil query activeIndex onPick onClose />`

- [ ] **Step 1: Write the failing tests**

`useEntryCatalog`: returns entries only from the selected bundles; falls back to `enabledDefault` bundles when `selected` is empty.

`EntryMenu`: filters by query on name and description; Enter picks the active entry; Escape calls `onClose`.

`Composer`: `/` at the start opens the menu, `/` mid-message does not, Enter with the menu open picks rather than sending, Enter with it closed sends.

- [ ] **Step 2: Run them to verify they fail**

```bash
cd clients/web && bun run test EntryMenu useEntryCatalog Composer
```

Expected: FAIL.

- [ ] **Step 3: Implement**

`Composer` tracks whether the text matches `/^([/@])(\S*)$/` on the first line and, when it does, renders `EntryMenu` above the field and routes Arrow/Enter/Tab/Escape to it before its own handlers. Picking sets the text to `` `${sigil}${name} ` `` and refocuses the textarea.

- [ ] **Step 4: Run the tests**

```bash
cd clients/web && bun run test
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/
git commit -m "web: type / or @ to pick a command, skill or agent"
```

---

### Task 9: Docs, full verification, PR

**Files:**
- Modify: `docs/guide/skills-and-plugins.md`

- [ ] **Step 1: Update the guide**

Rewrite the slash-commands paragraphs added by PR #220: no `` !`cmd` ``, no `allowed-tools`, `@name` for agents, and the fact that a bundle's entries are listed on the Skills settings page.

- [ ] **Step 2: Full verification**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
cd clients/web && bun run test && bun run build && cd ../..
```

Expected: all green. `cargo fmt` runs before clippy — clippy fails on unformatted code.

- [ ] **Step 3: Push and update the PR**

```bash
git push --force-with-lease origin feat/slash-catalog
```

Then retarget PR #220's head to this branch, or open a replacement PR, and rewrite the body to describe the catalogue design.

## Self-Review

**Spec coverage:** catalogue at ingest → Task 3; `catalog` column and `skill_count` removal → Task 4; lazy backfill and `PluginView` → Task 5; no new endpoint → satisfied by Task 5 (nothing added to `http/mod.rs`); expansion from the DB, both sigils, escaping, `start_blocked` fix, `!cmd` and `allowed-tools` removal → Tasks 2 and 6; `UserPromptExpansion` carrying kind → Task 6; runtime scan deletion → Task 1; skills page → Task 7; composer → Task 8; guide → Task 9. Failure modes are covered by tests in Tasks 4 (NULL/corrupt column), 5 (missing artifact), 6 (unknown name, no plugin service).

**Placeholder scan:** no TBD/TODO; every step names its command and expected result.

**Type consistency:** `CatalogEntry` (stored, with `template`) and `CatalogEntryView` (wire, without) are distinguished at every use. `catalog::build`, `catalog::frame`, `skills::parse`, `commands::parse_invocation(prompt, sigil)`, `commands::expand(template, args)` and `PluginProvisioner::catalog` keep the same signatures in Tasks 2, 3, 5 and 6.
