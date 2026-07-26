# Local runtime loads the CLI-installed plugin library

**Status:** approved 2026-07-26
**Scope:** `horsie connect` (CLI) + docs. No runtime, server, or protocol changes.

## Problem

horsie has two skill/plugin sources:

1. **Server DB-managed bundles** — installed via web UI / `POST /api/plugins`,
   selected per session, materialized into the runtime by env-injected fetch
   (`HORSIE_PLUGIN_MANIFEST` + `HORSIE_PLUGINS_DIR`). Works end-to-end on the
   **velos** vendor only.
2. **CLI host library** — `horsie plugin install` clones a git repo into
   `storage.plugins_dir` (+ `plugins.json` lockfile). Consumed only by the
   daemon/job path, which passes `--plugins-dir` to the runtimes it spawns.

A local runtime connected to the session server via `horsie connect`
(`/api/runtime/connect`, vendor kind `local`) gets **neither**: `connect.rs`
spawns `horsie-runtime` with only `--endpoint/--runtime-id/--workspace`, and
per-session env injection can't reach an already-running dialed-in process.
Sessions on the local vendor therefore see zero skills.

## Decision (from brainstorming)

- Install stays in the CLI: existing `horsie plugin install/list/update/remove`
  commands, unchanged (no rename, no `skill` alias).
- The connected local runtime exposes the **whole host library** to **every
  session it runs** (all-or-none), like the daemon/job path — not per-session
  server-bundle selection. Server bundles remain velos-only.

## Design

### Architecture

One skill source per runtime, decided at spawn. The runtime already prefers
`plugins_fetch::provision_plugins()` (env manifest, velos path) and falls back
to the `--plugins-dir` flag (`runtime/src/main.rs:149-151`). `horsie connect`
simply starts supplying the fallback. The server is untouched: the local
vendor keeps `supports_provisioning: false` and the web bundle picker stays
hidden for it.

Skills surface through the existing relays, no new machinery:

- Workspace scan: `scan::shared_skills` runs `plugins::discover_skills` per
  scan request when `include_shared` is set → newly installed plugins appear
  on the **next scan, no reconnect needed**.
- SessionStart hooks: relayed from `registry.plugins_dir()` at session start
  (`runtime/src/main.rs:339-343`).

### Components

**`cli/src/connect.rs`** — `run()` gains `plugins_dir: Option<PathBuf>` and
`hook_path: Vec<PathBuf>` parameters. When `plugins_dir` is `Some`, it appends
`--plugins-dir <dir>` and one `--hook-path <dir>` per entry to the
`horsie-runtime` command line. The connection summary line notes the library,
e.g. `· plugins: 3 skills from /home/user/.local/share/horsie/plugins`
(count via the lockfile; omit the note when no library).

**`cli/src/main.rs`** (connect dispatch) — loads the horsie config and mirrors
the daemon path (`cli/src/daemon/mod.rs:128-130`) exactly:

- `plugins_dir = plugins::plugins_dir_if_populated(&cfg.storage.plugins_dir)`
  (empty/missing library → `None` → flag omitted, runtime behaves as today)
- `hook_path = plugins::resolve_hook_path(cfg.runtime.hook_path)` **only when**
  `plugins_dir.is_some()`, empty vec otherwise

**Docs**

- `docs/guide/skills-and-plugins.md` — currently states the local runtime
  doesn't install bundles. Reframe the two sources: server bundles
  (web-managed, per-session) = velos only; host library
  (`horsie plugin install`) = local runtime (`horsie connect`) + daemon jobs,
  applies to every session on that machine.
- `docs/guide/runtime-vendors.md` — update the vendor comparison row for the
  local vendor accordingly.

### Data flow

```
horsie plugin install <git-url>     # existing: clone → plugins_dir + lockfile
horsie connect --server ...         # NEW: passes --plugins-dir/--hook-path
  └─ horsie-runtime --plugins-dir <host library>
       └─ exposes library as read-only horsie_shared workspace
            ├─ scan relay → skills listed in every session on this runtime
            └─ SessionStart hooks run on the host at session start
```

### Error handling

No new failure modes. Empty or missing library → flag omitted → runtime
unchanged. Unreadable plugin dirs are already skipped best-effort by
`discover_skills` / `run_session_start`. The CLI adds no I/O beyond what the
daemon path already does.

### Trust note

Host-library SessionStart hooks execute on the user's own machine; the
`horsie connect` path is unsandboxed today (no `--sandbox-caps` plumbed
through). This matches the daemon/job trust model — the user installed the
plugins themselves — and the guide states it explicitly.

### Testing

- **Unit (TDD):** connect command-building with and without a plugins dir;
  hook-path passed only when plugins present; summary-line rendering; config
  resolution in the dispatch (mirroring daemon tests).
- **E2E:** the existing Playwright harness (`clients/web/e2e/`) drives a real
  local runtime daemon — add a case asserting a session on the local vendor
  sees a host-library skill (runtime spawned with `--plugins-dir` pointed at a
  fixture plugin).
- **Manual:** on the homelab, `horsie plugin install` a skill, `horsie
  connect`, start a session on the local vendor, confirm the skill appears;
  install a second skill without reconnecting and confirm it appears on the
  next session.

## Out of scope

- Per-session bundle selection on the local vendor; local runtimes fetching
  server bundles (would require connect-protocol changes — deliberately
  rejected in favor of the host-library model).
- Renaming or aliasing the `plugin` commands.
- Sandboxing the `horsie connect` runtime.
- Any server, runtime, or wire-protocol changes.
