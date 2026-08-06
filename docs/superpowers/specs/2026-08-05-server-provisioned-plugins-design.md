# The server owns skills; the CLI only runs them — design

Written 2026-08-05. Removes plugin and marketplace management from the CLI and
makes a session's server-selected bundles the only source of skills a runtime
sees.

## Why

Plugin management exists twice. `horsie plugin install|list|update|remove` and
`horsie marketplace add|list|show|update|remove` maintain a library under
`<data_dir>/plugins`, which `horsie connect` hands to its runtimes as
`--plugins-dir`. Since #210 the server has all of it too — marketplaces, ingest,
artifacts — and since #178 a session selects its own bundles, which the runtime
fetches at startup.

Two libraries, one machine, and the wrong one wins by accident. `runtime/src/main.rs:229`
prefers fetched bundles and falls back to the host library, so selecting skills
for a session *replaces* the local library, and selecting none silently restores
it. Which set of skills an agent has depends on a choice made in a web form
about a directory the web UI cannot see.

The server side is also the only one that scales: it is per-user, it is what the
hosted product will use, and a remote sandbox has no host library to fall back
on. So the CLI's copy goes.

Three things this change deliberately does not fix, filed instead: #242 (a
hibernated runtime replays an expired artifact token), #243 (a session deleted
while its vendor is offline leaks the runtime state dir), #244 (bundles are not
shared between sessions).

## What is deleted

- The `horsie plugin` and `horsie marketplace` command trees, `cli/src/plugins.rs`
  and `cli/src/marketplace.rs`.
- `storage.plugins_dir` and `storage.data_dir`. `data_dir` exists only to hold
  `plugins/`, `sources/` and `marketplaces/`; with those gone it has no reader.
- `connect::PluginLibrary`, `RuntimeVendor::with_host_library`, the
  `host_library` and `host_sources` fields, and
  `horsie_support::plugin::grants::plugin_library_grants`.
- `RuntimeConfig.plugins_dir` (`models/fluorite/executor.fl`), the runtime's
  `--plugins-dir` flag, and the arg in `process_provider.rs`. That field is only
  ever set from `host_library` (`runtime-vendor/src/vendor.rs:826`), so once the
  host library is gone it is dead wire.
- `ENV_PLUGINS_CACHE` and `BundleDelivery.cache_dir`, along with `copy_dir` in
  `runtime/src/plugins_fetch.rs` — see "No cache" below.

There is no migration for an existing `<data_dir>/plugins`. `horsie connect`
prints one line saying local plugins are no longer read and the directory can be
deleted.

The CLI cannot create sessions or set their skill selection today — `horsie session`
is `tail`, `list` and `status` — so nothing is left needing a read-only
`plugin list` to replace what is removed.

**What survives:** `runtime.hook_path` and `resolve_hook_path`'s `node`
auto-discovery. Both live in `cli/src/plugins.rs` today and move to
`cli/src/connect.rs`, its only remaining caller — a file is not worth keeping for
two functions. `library_for_runtime` is deleted rather than moved: it resolves a
hook path only when the *host* library is populated, so a user with no local
plugins and server bundles selected gets no interpreter and their bundles' hooks
cannot run. `connect` resolves the hook path unconditionally instead.

## Layout

```
<state_dir>/
  runtimes/<runtime_id>/    spec.json, capabilities.json, agents.json   (unchanged)
  plugins/<runtime_id>/     one unpacked bundle per directory, plus .manifest.json
```

`BundleDelivery.dir` becomes a root, and the vendor appends the runtime id when
it sets `ENV_PLUGINS_DIR` (`vendor.rs:813`). Today it passes `b.dir.clone()`
verbatim, so **every runtime on a vendor shares one plugins directory**. A
session that provisions any bundle scans the whole directory, so it sees every
other session's skills; two sessions holding different versions of the same
plugin name overlay each other's files, because `copy_dir` merges into an
existing destination without clearing it. Appending the runtime id is the whole
fix.

`plugins/` is a sibling of `runtimes/`, not nested inside it, so cleanup can
remove a runtime's bundles without touching the spec file that decides whether
that runtime is revivable.

## No cache

An earlier draft had a content-addressed cache — `plugin-cache/<sha256>`, with
each runtime's directory holding symlinks into it. It is dropped. It needs
reference counting and an age policy for eviction, unpack-to-temp plus atomic
rename for concurrent unpacks of the same hash, and its own sandbox read grant
because Landlock and Seatbelt resolve *through* symlinks. It would also make the
local vendor's materialization differ from a remote sandbox's, where the machine
is ephemeral and a shared cache buys nothing.

Dropping it costs little, because the case that mattered is handled by the
lifecycle below: a runtime's bundles survive its process, so waking a hibernated
session re-fetches nothing. What remains is duplication between *distinct*
sessions, which is #244.

## Materializing, once

`runtime/src/plugins_fetch.rs` keeps its contract — read the manifest from the
environment, fetch each artifact over the runtime's own outbound connection,
verify the sha256 — and changes in three ways.

**It unpacks in place.** Straight into `<plugins_dir>/<name>`. No cache
parameter, no `copy_dir`.

**It clears the directory first.** Whatever the last materialization left is
removed before this one starts, so a session sees exactly its manifest even when
a bundle fails.

**It records what it materialized.** After a run in which *every* ref landed, it
writes `<plugins_dir>/.manifest.json` holding the manifest verbatim. On startup,
if that file matches the current manifest byte for byte, the whole fetch is
skipped. The marker can live inside the scanned directory because
`plugin_dirs` (`runtime/src/plugins.rs:28`) keeps only entries that are
directories, so a file is never mistaken for a plugin.

That marker is what makes provisioning once-per-runtime rather than
once-per-process. The vendor re-spawns a hibernated runtime by calling
`provision()` with the persisted spec (`vendor.rs:657`) — it must, the process
died — and the new process re-runs `provision_plugins()`. The marker makes that
second run a stat and a compare. It also gives two properties the current code
lacks: a changed selection re-materializes cleanly, and a partial failure leaves
no marker, so the next start retries the bundles that were missing instead of
losing them for the life of the session.

Fetching stays best-effort per bundle: one unavailable skill does not fail a
session.

`provision_plugins` returns the directory whenever the manifest was non-empty,
rather than only when at least one bundle landed. With no host library there is
nothing to fall back to, so the old distinction no longer decides anything.

## Cleanup

A runtime's bundles live exactly as long as its *identity*, not its process.

- `DeleteRuntime` removes `plugins/<runtime_id>` alongside the
  `remove_dir_all(runtimes/<id>)` it already does (`vendor.rs:692`). The session
  is gone.
- Vendor startup removes any `plugins/<id>` with no `runtimes/<id>/spec.json`.
  Nothing is live at boot, so anything without a spec is crash debris — and the
  spec is the same record that decides whether the runtime can be revived at
  all, so the two can never disagree.
- `halt()` removes nothing. Stopping a process is not losing a session.

No sweep timer, no age policy, no reference counting. Orphan detection is not
attempted: a session deleted while the vendor was offline leaves both directories
behind, because `RuntimeManager::delete` is advisory and there is no
vendor→server reconciliation. That is #243.

## Sandbox

The sandbox is applied before the runtime's async body runs, so the fetch and
unpack happen confined. `write_caps_file` grants the host library, `sources/`,
the hook path, and — when respawnable — `runtimes/<id>`. It does not grant the
bundles directory, and the baseline spec is static system paths. `horsie connect`
defaults to sandbox on, so the unpack should be failing today, and because
provisioning is best-effort it fails to "no skills" without saying so.

Replacing the `plugin_library_grants` call with:

- `Dir(plugins/<runtime_id>, ReadWrite)` — where the runtime unpacks.
- the hook path directories, `Read` — now unconditional rather than gated on a
  populated host library.

Network already allows egress, deliberately, so the fetch itself is fine.

## Testing

**runtime** — a manifest matching `.manifest.json` skips the fetch entirely (no
HTTP); a changed manifest clears and re-materializes; a bundle dropped from the
manifest disappears; a partial failure writes no marker and the next start
retries; the unpack lands in `<dir>/<name>`.

**vendor** — `ENV_PLUGINS_DIR` differs per runtime id; `DeleteRuntime` removes
the runtime's plugins directory; boot removes a plugins directory with no
`spec.json` and keeps one that has it; `halt` keeps it; the capability file
grants the plugins directory and the hook path.

**cli** — the command trees are gone (compile-level, once the config keys are
removed); `connect` passes a hook path with no host library.

**e2e** — `clients/web/e2e` already covers skills reaching a session over the
non-sandboxed `e2e` vendor, which guards the wiring. The sandboxed `horsie connect`
path cannot run in CI, so the grant change is covered by asserting the written
capability file, plus a manual verification note in the PR.
