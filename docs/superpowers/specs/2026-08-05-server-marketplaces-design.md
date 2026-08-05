# Server-side marketplaces, and one box that installs anything — design

Written 2026-08-05. Closes Phase 0 PR3 of #105, and closes a parity gap
underneath it that nobody had written down.

## Why

Two problems, one of which is invisible in the issue.

**The stated one.** Installing a bundle through the web UI means pasting a git
URL. The CLI has had marketplaces since PR2 (#113) — `horsie marketplace add`,
then `horsie plugin install <plugin>@<marketplace>` — and the server has
nothing. A server user cannot browse a catalogue; they have to already know the
URL of the thing they want.

**The one that matters more.** `server/src/plugins/ingest.rs:52` clones a repo
and calls `PluginRoot::inspect` on the **repo root**. It never reads
`marketplace.json`, and it has no concept of a subpath. `cli/src/plugins.rs:345`
does the opposite: a plain URL is checked for a marketplace index, and the index
says where the plugin actually is.

So **`pbakaus/impeccable` — the repo that motivated all of #105 — installs from
the CLI and fails from the web UI**, because its `marketplace.json` declares
`"source": "./plugin"`. Every marketplace-shaped repo fails the same way, and
that is a large share of the ecosystem. A marketplace picker cannot work until
this is fixed: the most common entry shape is exactly the one ingest cannot
handle.

Both ship together. The parity fix alone would be worth a PR; shipping the
picker without it would be building a browser for a catalogue whose contents
cannot be installed.

## What the CLI already settled

Not re-litigated here. The resolution logic is already shared in
`horsie-support` and the server should call it rather than grow its own:

- `Marketplace::read(repo_root)` — parse `.claude-plugin/marketplace.json`,
  returning entries plus the malformed ones it skipped.
- `source_location(source, marketplace_url, marketplace_ref)` — reduce all four
  `source` forms to one `(url, git_ref, subpath)`.
- `join_declared(root, declared)` — resolve a `./`-prefixed declared path.
- `PluginRoot::inspect(dir)` — read the manifest, find the skills.

Three semantics carry over from the CLI verbatim, because a user with both
surfaces should not have to learn two models:

- **Removing a marketplace leaves plugins installed from it in place.** Dropping
  a source is not dropping the software.
- **A marketplace declaring exactly one plugin auto-selects it.** This is the
  impeccable case.
- **An installed plugin remembers `marketplace` and `marketplace_entry`,** and
  `update` re-resolves through the index — a plugin the catalogue has since
  moved or re-pinned follows along. The index's name for an entry is not always
  the name it installs as (`42crunch-api-security-testing` installs as
  `api-security-testing`), which is why both are recorded.

## One box

The design decision that shapes everything else: **there is one input, and the
server works out what you gave it.** A person pasting a URL should not have to
know first whether it is a bundle or a catalogue — that is the server's job, and
the server has to clone the repo to find out either way.

`POST /api/plugins` therefore stops always returning a `PluginView` and returns
a union:

```fluorite
#[type_tag = "outcome"]
union InstallOutcome {
    /// A plain bundle repo, or a marketplace declaring exactly one plugin.
    Installed(PluginView),
    /// Several plugins on offer: the source is recorded and its index cached,
    /// and the caller picks from `MarketplaceView.plugins`.
    Marketplace(MarketplaceView),
}
```

One clone, one round trip, and the outcome is either "it is installed" or "here
is a source I added and what is in it". No preflight endpoint, no classify-then-
install pair, and no error the user has to translate into an action.

**Recording the marketplace on paste is deliberate, and is not a hidden side
effect.** It lands in a Marketplaces section on the page the person is already
looking at, immediately, alongside the box they just typed into. The alternative
— show the entries but record nothing until something is installed — means
re-cloning to come back to it later, and a "sources" list that cannot show you
the source you are currently browsing.

Installing from the picker reuses the same endpoint with the other half of the
input:

```fluorite
struct PluginInstallInput {
    /// One of: a git URL, or a (marketplace, plugin_name) pair.
    source_url: Option<String>,
    source_ref: Option<String>,
    marketplace: Option<String>,
    plugin_name: Option<String>,
}
```

`source_url` becomes optional, so the two forms are expressed in one type. That
is a weaker contract than a union would give — `{url: None, marketplace: None}`
is representable and must be rejected at runtime — and it is the deliberate
trade: `PluginInstallInput` is an existing wire type with an existing client,
and a union here would buy compile-time safety over a two-field validation in
exchange for reshaping every call site. Recorded as a decision rather than an
oversight; see "Rejected alternatives".

## Ingest parity

`ingest_git` grows one step between clone and inspect, and one parameter.

```rust
pub struct IngestTarget {
    pub url: String,
    pub git_ref: Option<String>,
    /// Where inside the checkout the plugin root sits. `None` means the repo
    /// root, which is what a plain bundle repo means.
    pub subpath: Option<String>,
}

/// What a cloned repo turned out to be.
pub enum Ingested {
    /// `PluginBundle` is today's `Ingested` struct renamed — `name`, `version`,
    /// `description`, `skill_count`, `has_hooks`, `unsupported_hooks`,
    /// `zip_bytes`, `hash` — unchanged except that it now also carries the
    /// `subpath` it was resolved from, so `update` re-resolves to the same tree.
    Plugin(PluginBundle),
    /// The repo is a catalogue, not a bundle. Never returned when `subpath` was
    /// given: the caller already resolved through an index, and asking again
    /// would mean the index pointed at another index.
    ///
    /// Carries `horsie_support::plugin::Marketplace` (its declared `name`,
    /// `plugins` and `skipped`) plus the resolved `sha`, which is everything
    /// the `marketplaces` row stores.
    Marketplace(ParsedMarketplace),
}
```

`ingest_git` then:

1. clones `url` at `git_ref` into a tempdir, as today;
2. if `subpath` is given, `join_declared`s it and inspects there — the marketplace
   case, already resolved;
3. otherwise reads `Marketplace::read(&dest)`:
   - no index → inspect the repo root, exactly as today;
   - index with one entry → resolve it via `source_location` and inspect there.
     If the entry points at a **different repo**, that repo is cloned; a `Path`
     entry stays inside the checkout we already have;
   - index with several entries → return `Ingested::Marketplace`.

The zip is packed from the resolved plugin root, not the repo root, so a bundle
carries its own tree and nothing else. This is also what makes the artifact hash
stable for a plugin whose repo holds several.

**A malformed entry is skipped, not fatal** (`Marketplace.skipped` already
carries them), so one broken entry cannot make a 276-entry catalogue
unbrowsable. The skipped names are surfaced on the marketplace row so they are
visible rather than silently dropped.

## Data model

```sql
-- server/migrations/{sqlite,postgres}/0022_marketplaces.sql
CREATE TABLE marketplaces (
    name          TEXT PRIMARY KEY,   -- the index's own `name`, else repo basename
    source_url    TEXT NOT NULL,
    source_ref    TEXT,
    sha           TEXT,               -- HEAD when last read, so refresh can report a no-op
    plugin_count  INTEGER NOT NULL DEFAULT 0,
    entries       TEXT NOT NULL,      -- the parsed index, JSON
    skipped       TEXT NOT NULL,      -- malformed entry names, JSON array
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

ALTER TABLE plugins ADD COLUMN marketplace TEXT;
ALTER TABLE plugins ADD COLUMN marketplace_entry TEXT;
```

`entries` is the **cached parsed index**, and browsing is a local read. The
official marketplace has ~276 entries; re-cloning per page view would put a git
clone on the render path. The cost is that the cache is a snapshot: a plugin
published after the last refresh does not appear until `POST /:name/refresh`,
which re-clones, re-parses and updates `sha`.

Storing the parsed entries rather than the raw file means a `marketplace.json`
schema change is absorbed at refresh time by the same parser the CLI uses,
rather than at read time by a second one.

`marketplace` / `marketplace_entry` on `plugins` are nullable because a bundle
installed from a plain URL has no marketplace, and every row that exists today
is exactly that.

## API

```
GET    /api/marketplaces                 → Vec<MarketplaceView>
DELETE /api/marketplaces/:name           → 204; installed plugins untouched
POST   /api/marketplaces/:name/refresh   → MarketplaceView (re-clone, re-parse)
POST   /api/plugins                      → InstallOutcome   (the one box)
```

There is deliberately **no `POST /api/marketplaces`**. Adding a source happens by
pasting its URL into the one box, which is the whole point of the design; a
second way in would be a second thing to keep consistent.

`MarketplaceView` carries its entries, so the list endpoint answers the picker
too and there is no `GET /:name/plugins` — a deviation from the PR3 sketch in
`2026-08-02-plugin-marketplace-design.md`, which listed one. The entries are
already cached on the row, so a second endpoint would be a second read of the
same column:

```fluorite
struct MarketplacePluginView {
    name: String,
    description: Option<String>,
    version: Option<String>,
    /// True when a bundle installed from this entry is already in the library,
    /// so the picker can say "installed" instead of offering it again.
    installed: bool,
}

struct MarketplaceView {
    name: String,
    source_url: String,
    source_ref: Option<String>,
    plugin_count: u32,
    updated_at: String,
    plugins: Vec<MarketplacePluginView>,
    /// Entries the index declared that could not be parsed. Shown rather than
    /// dropped: a catalogue that quietly lost three plugins is a bug report
    /// nobody files.
    skipped: Vec<String>,
}
```

Routes sit beside `/api/plugins`, not under `/api/admin/` — bundle install
already accepts arbitrary git URLs at this trust level, and adding a catalogue
of URLs does not change it.

## Web

Settings → Skills owns all three jobs. **No new nav entry**: marketplaces are a
way of getting skills, not a separate concern, and splitting them across two
pages would mean the one box lives on one page and its result on another.

Three sections, top to bottom:

1. **Install** — one URL field plus an optional ref, unchanged from today except
   for what happens on success. `Installed` refreshes the bundle list;
   `Marketplace` reveals its row in section 2, expanded.
2. **Marketplaces** — one row per source: name, plugin count, last updated,
   refresh and remove. Expanding a row lists its plugins with a filter box (the
   276-entry case is the reason the filter exists) and an install button per
   entry, disabled with "installed" where the library already has it.
3. **Installed bundles** — as today, plus a chip naming the marketplace a bundle
   came from when it has one.

Removing a marketplace warns that installed bundles stay, matching `horsie
marketplace remove`.

The page is at 254 lines today and would roughly double. Sections 2 and 3 become
their own components (`MarketplaceRow.tsx`, the existing `BundleRow`), leaving
`SkillsSettings.tsx` as composition rather than markup.

## Error handling

- **Ambiguous or unknown plugin within a marketplace** — error naming the
  available entries, as the CLI does.
- **Unresolvable external source** — error naming the entry and the URL that
  failed to clone, so a broken third-party entry is distinguishable from a
  broken marketplace.
- **A URL that is neither a bundle nor a marketplace** — the existing "provides
  no skills" error, which #206 already taught to name unrunnable hook events too.
- **Re-pasting a marketplace already added** — not an error. It refreshes the
  cached index and returns the same row, because "add it again" and "refresh"
  are the same intent from the user's side.
- **A name collision between two marketplaces** — rejected, naming the existing
  source URL. `name` is the primary key, and silently renaming one would break
  the `marketplace` provenance already recorded on installed plugins.

## Testing

- **`horsie-support`** — no new tests. The resolution logic is unchanged and
  already covered; this design's claim is that the server calls it.
- **ingest** — a `file://` git fixture per shape: plain bundle repo; marketplace
  with one entry (the impeccable shape: index at the root, plugin at `./plugin`,
  non-default skills path); marketplace with several entries returning
  `Marketplace` rather than installing; a `Git`-sourced entry pointing at a
  second repo; a malformed entry skipped while its siblings resolve. **The
  one-entry case is the regression test for the bug this PR exists to fix.**
- **store** — the round trip of `entries`/`skipped` through JSON, and that
  deleting a marketplace leaves plugins with that `marketplace` value intact.
- **http** — `POST /api/plugins` returns each `InstallOutcome` arm; installing
  by `(marketplace, plugin_name)` resolves through the *cached* index without
  cloning the marketplace again; a request with neither `source_url` nor a
  marketplace pair is rejected.
- **web** — Vitest over the picker: entries already installed are disabled, the
  filter narrows a long list, and a `Marketplace` outcome from the install box
  reveals the source expanded.
- **e2e** — one Playwright case over a `file://` marketplace fixture: paste the
  URL, get a source rather than an install, pick a plugin, see it in the
  installed list. This is the flow the whole design exists to produce, and it is
  the only test that covers all three sections together.

## Rejected alternatives

- **A separate Settings → Marketplaces page.** Recommended early and dropped:
  the one box and its result would live on different pages, so pasting a
  catalogue URL into Skills would have to navigate somewhere else to show what
  happened.
- **A preflight `POST /api/plugins/resolve`.** Classify, then install. An extra
  round trip and a second clone on every install, to avoid a union response.
- **A typed `is_marketplace` error the client turns into an action.** Cheaper
  than a preflight and considered seriously, but it makes the common case an
  error: pasting a catalogue URL is a reasonable thing to do, not a mistake.
- **A `PluginInstallInput` union over the two input forms.** Stronger typing than
  four optional fields, at the cost of reshaping an existing wire type and its
  client for a constraint one runtime check covers.
- **A persistent `sources/` clone directory on the server**, as the CLI has.
  Faster refresh (`pull --ff-only` rather than a clone) and it would let
  `ensure_checkout` be reused verbatim, but it gives the server a new on-disk
  state directory to manage, back up and garbage-collect. Ingest deliberately
  uses throwaway tempdirs today; the cached index is what makes that affordable.

## Out of scope

- **The CLI and server marketplace registries stay separate stores.** One is
  per-user on a host, the other is shared server state. They share resolution
  logic and semantics, not rows.
- **Marketplace auth.** Private catalogues need credentials the server does not
  have a model for; every marketplace in the wild today is a public repo.
- **Automatic refresh.** No polling, no TTL — refresh is a button. A scheduled
  refresh is a routines job if it is ever wanted.
- **Update-available badges.** Provenance makes it possible to compare an
  installed bundle against its index entry, but doing it well means version
  comparison across four source forms.
