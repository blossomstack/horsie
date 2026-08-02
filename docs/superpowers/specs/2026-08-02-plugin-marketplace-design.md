# Plugin manifest resolution and marketplaces

## Context

`horsie plugin install https://github.com/pbakaus/impeccable` fails with
`'…' does not expose any SKILL.md; not a skills plugin`, even though the
runtime's own loader would load that plugin without complaint. The immediate
cause is a narrow filesystem heuristic in the CLI. The underlying cause is
that horsie parses the Claude Code plugin manifest in three different places,
with three different levels of fidelity, and none of them understands
marketplaces.

This spec covers Phase 0 of issue #105: make plugins installable. It does not
cover hooks beyond `SessionStart`, agents, commands, or MCP servers — those
are Phases 1–4 of that issue and are deliberately out of scope here.

### What the ecosystem actually standardises

Worth stating explicitly, because it determines how much of Claude Code's
model horsie should adopt:

- **Skills are a real cross-harness standard.** The [Agent Skills
  specification](https://agentskills.io/specification) defines the
  `skill-name/SKILL.md` directory layout and the frontmatter fields (`name`,
  `description`, `license`, `compatibility`, `metadata`, `allowed-tools`),
  plus optional `scripts/`, `references/`, `assets/`. It defines **nothing**
  about packaging, distribution, plugins, bundles, or registries.
- **Plugins and marketplaces are Claude Code's, with one adopter.**
  `.claude-plugin/plugin.json` and `.claude-plugin/marketplace.json` are
  Claude's design. Grok Build reads them for compatibility rather than having
  designed its own. Gemini CLI's "extensions" (`gemini-extension.json`) are a
  separate, incompatible concept. No other harness has a marketplace at all.

So there is no neutral plugin standard available to adopt instead. horsie
adopts Claude's format as a **read-only input** — it parses `plugin.json` and
`marketplace.json`, and never writes them.

### Existing architecture

- `runtime/src/plugins.rs` is the loader. `discover_skills()` globs
  `<loc>/*/SKILL.md`, where `<loc>` comes from the manifest `skills` field
  (string or array) or defaults to `skills/`. It is manifest-aware and
  correct, including `./`-prefixed paths. `plugin_dirs()` treats every direct
  child of `plugins_dir` as a plugin root.
- `cli/src/plugins.rs` is the host-library installer (`horsie plugin
  install/list/update/remove`). Its `has_skills()` gate checks only
  `<root>/skills/*/SKILL.md`, `<root>/*/SKILL.md` and `<root>/SKILL.md`. It
  never reads `plugin.json`.
- `server/src/plugins/ingest.rs` is the bundle installer: clone, inspect,
  deterministically zip, hash. Its `inspect_plugin_dir()` is a third,
  independent reimplementation — manifest-aware for `skills`, but with its
  own quirks (it detects hooks by substring-matching `"SessionStart"` in the
  raw file).
- Two delivery paths reach a runtime: server bundles (zip artifacts fetched
  by `runtime/src/plugins_fetch.rs`, velos only) and the host library
  (`horsie connect`, passing `--plugins-dir`).

### Why impeccable fails

`pbakaus/impeccable` is a marketplace repo. Its
`.claude-plugin/marketplace.json` declares one plugin with
`"source": "./plugin"`, and its root `.claude-plugin/plugin.json` declares
`"skills": "./.claude/skills/"`. The actual `SKILL.md` therefore sits at
`.claude/skills/impeccable/SKILL.md` — two levels below anything
`has_skills()` inspects. The runtime would find it; only the install gate
rejects it.

## Goals

1. Installing a plugin from a repo works whether the plugin is at the repo
   root, at a subpath, or declared by a marketplace.
2. `.claude-plugin/plugin.json` is parsed in exactly one place.
3. Marketplaces are first-class: added as persistent sources, browsed, and
   installed from — via both the CLI and the web UI.
4. No behaviour change for plugins that install correctly today.

## Non-goals

Hook events beyond `SessionStart`; agents; commands; `.mcp.json`; changes to
the bundle zip/artifact mechanism; a hosted horsie catalog.

## Design

### The shared crate: `horsie-support`

A new workspace member, `support/` → crate `horsie-support`. It is the
hand-written counterpart to `horsie-models` (which holds only
fluorite-generated wire types) and is depended on by `cli`, `server` and
`runtime`.

Modules:

- `plugin::manifest` — `.claude-plugin/plugin.json`.
- `plugin::marketplace` — `.claude-plugin/marketplace.json`, normalising the
  source forms.
- `plugin::skills` — manifest-aware skill location and enumeration; the logic
  currently triplicated.
- `plugin::layout` — "is this directory a plugin root?".
- `git` — clone/fetch helpers, behind a non-default `git` cargo feature.

`cli` and `server` enable `git`; `runtime` does not. The runtime only ever
reads already-materialised trees, and it ships into the sandbox, so its
dependency surface is kept minimal.

Putting git behind a feature in this crate rather than leaving it in the
callers is deliberate: external marketplace sources need cloning from both
the CLI and the server, and leaving that in the callers would recreate
exactly the duplication this crate exists to remove.

**Guard rule against grab-bag rot:** every item lives under a domain module;
nothing is exported at the crate root; a module that acquires its own heavy
dependencies graduates into its own crate.

### Manifest model

`PluginManifest` carries `name`, `version`, `description` and `skills`. The
`skills` field accepts a string or an array of strings, each relative to the
plugin root; absent means `skills/`. Fields for agents, commands, hooks and
`mcpServers` are **not** modelled yet — Phases 1–3 add them here, in one
place, which is the point of the consolidation.

### Marketplace model

`.claude-plugin/marketplace.json` lists plugins, each with a `source`. Four
forms occur in the wild; a survey of `anthropics/claude-plugins-public` (276
entries) found all four, with 223 of the 276 pointing at *external* repos
rather than paths inside the marketplace:

| `source` in JSON                                | count | normalises to |
| ----------------------------------------------- | ----: | ------------- |
| `"./plugins/foo"`                                |    53 | `Path`        |
| `{source:"git-subdir", url, path, ref?, sha?}`   |    78 | `Git`         |
| `{source:"url", url, path?, sha?}`               |   143 | `Git`         |
| `{source:"github", repo, commit?, sha?}`         |     2 | `Git`         |

A marketplace is therefore an *index*, not merely a directory of
subdirectories. Both forms normalise into:

```rust
enum PluginSource {
    Path(String),                                    // relative to the marketplace clone
    Git { url: String, path: Option<String>, git_ref: Option<String> },
}
```

`{source:"github", repo}` maps to `https://github.com/<repo>.git`, with
`commit` used as the ref. The `sha` field is treated as opaque metadata and
ignored: it is an integrity digest over a packaging horsie does not
reproduce, so honouring it would mean claiming a verification we do not
perform.

### Resolution

Everything funnels through one pipeline: **source spec → resolve → a
directory that is a plugin root → install.**

Two entry points:

- `install_from_url(url, ref)` — clone; if `.claude-plugin/marketplace.json`
  is present, resolve its entries, auto-selecting when it lists exactly one
  and erroring with the available names when it lists several; otherwise the
  repo root must itself be a plugin root.
- `install_from_marketplace(plugin, marketplace)` — look up the cached
  marketplace clone, find the entry, resolve its `PluginSource` (a `Path`
  resolves inside the marketplace clone; a `Git` source clones that repo at
  its ref and descends into `path`).

A malformed *entry* is skipped with a warning rather than failing the whole
marketplace. One bad row must not brick a 276-entry index.

### On-disk layout

```
<data_dir>/sources/<key>/           full git clone (git pull works)
<data_dir>/plugins/<name>      ->   symlink to the resolved plugin root
<data_dir>/plugins/plugins.json     existing lockfile
<data_dir>/marketplaces/<name>/     marketplace clones
<data_dir>/marketplaces/marketplaces.json
```

`<key>` is a short hash of the normalised `(url, ref)` pair, not the plugin
name, so that a marketplace declaring several plugins as paths into its own
repo — or two plugins that happen to share an upstream repo — clone once and
share one working copy. Removing a plugin deletes its symlink and the lockfile
row; the clone under `sources/` is garbage-collected once no installed plugin
and no marketplace still points at it.

The layout is uniform: a plain plugin repo with no marketplace and no subpath
still clones to `sources/<key>/` and is symlinked from `plugins/<name>`, so
there is exactly one shape to reason about.

Installed plugins are **symlinks** into a full clone, not copies. Verified
against the existing loader: `read_dir` + `is_dir()` enumerate a symlinked
plugin dir, `glob` traverses it, and — critically — `strip_prefix` still
yields the correct `rel_dir`, because nothing in the discovery path
canonicalises. `PluginSkill::rel_dir` therefore stays relative to
`plugins_dir` instead of leaking the symlink target.

Symlinks preserve two things copying would lose: `git pull` on update, and
`current_sha()`, which shells `git -C <plugins_dir>/<name> rev-parse HEAD`
and resolves through the link into the real repo.

Marketplace clones live outside `plugins_dir` because `plugin_dirs()` and
`count_installed()` treat every direct child of `plugins_dir` as an installed
plugin.

### Sandbox grants

`cli/src/capabilities.rs::with_plugin_grants` appends read-only `Dir` grants
for the plugin library and hook interpreter dirs. Symlinked plugin roots
require the **target** to be granted too, since Landlock and Seatbelt both
resolve through symlinks — so the grant set becomes `plugins_dir`,
`sources/` and `marketplaces/`.

This surfaces a pre-existing bug that must be fixed as part of the same work.
`with_plugin_grants` is only called from the daemon path
(`cli/src/daemon/mod.rs:188`). Under `horsie connect`, the capability spec
arrives from the server and is written verbatim
(`runtime-vendor/src/vendor.rs:495`), while `host_library` is used only to
populate `RuntimeConfig.plugins_dir` (`vendor.rs:531`) and never contributes
a grant. So `horsie connect --sandbox` currently hands the runtime a
`--plugins-dir` it has no capability to read. The local vendor must inject
the library grants into the spec it writes.

### CLI surface

```
horsie marketplace add <url> [--name <n>] [--ref <r>]
horsie marketplace list
horsie marketplace update <name>
horsie marketplace remove <name>

horsie plugin install <url> [--name] [--ref] [--force]   # marketplace-aware
horsie plugin install <plugin>@<marketplace> [--force]
horsie plugin list | update <name> | remove <name>
```

SSH git URLs contain `@` (`git@github.com:x/y.git`), so the argument is
treated as `plugin@marketplace` only when it matches
`^[a-z0-9-]+@[a-z0-9-]+$`. That is unambiguous because the Agent Skills spec
constrains names to lowercase alphanumerics and hyphens, and no git URL form
matches it.

`plugins.json` entries gain an optional `marketplace` field. A new
`marketplaces.json` lockfile records added marketplaces (name, source URL,
ref, sha).

`plugin update` re-resolves from the recorded source: `git pull --ff-only` in
the clone under `sources/`, then re-point the symlink in case the plugin's
declared root moved.

### Server and web surface

- `models/fluorite/marketplaces.fl`: `MarketplaceView`, `MarketplaceAddInput`,
  `MarketplacePluginView`.
- `marketplaces` table: `name` PK, `source_url`, `source_ref`, `plugin_count`,
  `updated_at`.
- `GET|POST /api/marketplaces`, `DELETE /api/marketplaces/:name`,
  `POST /api/marketplaces/:name/refresh`,
  `GET /api/marketplaces/:name/plugins`.
- `PluginInstallInput` gains optional `marketplace` and `plugin_name`.
  Additive, so existing clients keep working.
- Web: a new `MarketplacesSettings.tsx` page beside `SkillsSettings.tsx` in
  the settings nav; Skills gains a browse-and-install picker over the plugins
  its marketplaces expose.

Marketplace management sits in Settings rather than Admin because bundle
install already lives there and already accepts arbitrary git URLs — the
trust level is unchanged.

### Error handling

- Ambiguous repo, and unknown plugin within a marketplace: error listing the
  available names.
- Unresolvable external source: error naming the entry and the URL it failed
  to clone.
- Malformed marketplace entry: warn and skip; the rest of the marketplace
  still resolves.
- The gate error that motivated this work is rewritten to state what was
  looked for and where, instead of the bare "does not expose any SKILL.md".

### Testing

- `horsie-support` unit tests over `tempfile` fixtures: each manifest shape,
  each of the four `source` forms, the name/URL disambiguation rule, and a
  regression fixture reproducing impeccable's exact layout (a marketplace
  pointing at `./plugin` **and** a root manifest with
  `skills: "./.claude/skills/"`).
- CLI tests install from a `file://` git fixture, so nothing touches the
  network.
- A symlinked-plugin discovery test in `runtime/src/plugins.rs`, pinning the
  `rel_dir` behaviour the layout depends on.
- The existing runtime and ingest tests are the guard that deleting the three
  duplicate parsers changes no behaviour; they must pass unmodified.

## Staging

Three stacked PRs, each independently shippable:

1. **`horsie-support` + manifest-aware install.** The crate, the three
   callers rewired onto it, marketplace *resolution* (no registry yet), and
   the sandbox grant fix. Closes the impeccable bug on its own.
2. **CLI marketplace registry.** `horsie marketplace add/list/update/remove`,
   `plugin install <plugin>@<marketplace>`, external source forms, the
   marketplaces lockfile.
3. **Server marketplaces + web UI.** Table, wire model, routes, and the two
   settings pages.

## Consequences

- `horsie-support` is depended on by published crates, so it must publish
  too. That needs a one-time crates.io trusted-publishing configuration
  before the next `v*` tag; the version-guard job will otherwise fail the
  release.
- Existing `plugins_dir/<name>/` directories are real clones, not symlinks.
  They keep working unchanged — `plugin_dirs()` does not care — so no
  migration is required; only new installs use the new layout.
- Phase 0 as described in issue #105 was smaller than this (it did not
  include a registry or UI). The issue is updated to match.
