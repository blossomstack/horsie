# Vendor agent restart resilience

Two independent failures, both triggered by a server redeploy, both rooted in
state a long-lived vendor agent captures once and never revisits.

## Symptoms

A session on the local vendor reports `This session can no longer run: no
runtime '<session-id>' on this vendor; it cannot be resumed`, permanently.

The same agent logs `vendor agent: attempt 5 failed: connect
wss://<server>/api/vendor/connect: HTTP error: 401 Unauthorized; reconnecting in
30.0s` forever, and only a manual restart clears it.

## Root causes

**The credential is resolved once.** `horsie connect` calls `resolve_token` at
startup (`cli/src/connect.rs:207`) and hands the resulting string to
`RuntimeVendor::run`, which reuses it for every dial for the life of the process
(`runtime-vendor/src/vendor.rs:271`). Access tokens live one hour
(`ACCESS_TOKEN_TTL_SECS`, `server/src/auth/service.rs:24`). An established
WebSocket is never re-authenticated, so the token dies unnoticed mid-link; the
next redial — whatever causes it — is the first time the corpse is presented,
and every subsequent one fails identically. The server restart does not
invalidate anything. It merely forces the first redial.

**Runtimes live only in memory.** The vendor tracks them in a `HashMap` behind a
mutex (`vendor.rs:148`) and kills every child process on shutdown via
`halt_all` (`vendor.rs:599`). Nothing is persisted, nothing is re-adopted at
startup. `GetRuntime` answers from that map alone (`vendor.rs:427`), so after a
restart every prior runtime is reported missing. The server maps that to
`VendorError::Gone` → `RuntimeError::Gone` (`runtime_manager.rs:203`) →
`ContextError::terminal` (`session_actor.rs:1228`) → a persisted
`SessionStatus::Unrecoverable` (`sessions/spec.rs:134`). Restarting `horsie
connect` therefore destroys every session on it, irreversibly.

That terminal treatment is deliberate and correct for velos, where re-creating
means a fresh container with a fresh clone. It is wrong for a vendor whose
workspaces are fixed user-owned directories: there is nothing to rebuild,
because nothing was ever built.

## Design

### 1. The agent refreshes its own credential

`RuntimeVendor::run` stops taking `token: Option<&str>` and takes a provider
invoked before every dial attempt:

```rust
pub type CredentialProvider =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Option<String>, CredentialError>> + Send + Sync>;

pub enum CredentialError {
    /// Could not reach the issuer. Retry with backoff.
    Transient(String),
    /// The credential is definitively dead. Stop.
    Dead(String),
}
```

`horsie connect` passes a provider that calls `resolve_token`; `velos-runtime`
passes one that returns its constant machine token. A `Transient` error is
treated exactly like a failed dial — logged, backed off, retried. A `Dead` one
ends `run` with an error, so the process exits non-zero telling the operator to
run `horsie auth login`.

`resolve_token` must therefore distinguish its two failure modes, which today
both collapse into `CliError::Server`:

- a network failure inside `post_json` propagates via `?` before the match
  (`cli/src/auth.rs:421`) — that is `Transient`;
- an HTTP error response from `/api/auth/refresh` reaches the `Err(_)` arm
  (`cli/src/auth.rs:434`), which already wipes the stored credential — that is
  `Dead`.

The fail-fast URL validation at the top of `run` stays; only the token moves to
per-attempt resolution.

**§2 must land first.** Exiting on a dead credential is only acceptable once a
restart is survivable; before §2, any exit destroys every session on the agent.

### 2. Local runtimes become durable

The vendor already owns a per-runtime directory,
`<state_dir>/<runtime_id>/capabilities.json` (`vendor.rs:652`). It grows two
files:

```
<state_dir>/<runtime_id>/
  capabilities.json   # existing sandbox capability spec
  spec.json           # RuntimeSpec as received on create (0600)
  agents.json         # per-agent cwd + env overlay, owned by the runtime process
```

`spec.json` is written by the vendor on create. `agents.json` is written by the
runtime process itself: `runtime/src/state.rs` holds the per-agent working
directory and env overlay in memory today, and gains a path (a new
`RuntimeConfig` field) to load from at startup and rewrite on each
`set_working_dir` / `set_env`. Those calls are rare and the file is small, so
write-on-change needs no batching.

A sandboxed runtime cannot write there by default. The baseline grants the
working dir read-write and `/usr`, `/bin`, `/etc` read
(`runtime-vendor/src/baseline.rs`), and nothing else — so `write_caps_file` must
add a read-write grant for the runtime's own state directory, alongside the
plugin-library grants it already injects. Without it the first `set_env` after
this change fails with a sandbox denial rather than an obvious error.

The in-memory map returns to meaning only "live right now". `GetRuntime`
resolves in three steps:

1. live in the map → return its transport;
2. not live, but `spec.json` exists → respawn from it, reload `agents.json`,
   return the new transport;
3. no directory → `Gone`, exactly as today.

Because disk is the source of truth, an agent restart is indistinguishable from
a hibernate. That is what stops Ctrl-C from destroying sessions.

`HibernateRuntime` changes from a no-op to: stop the process, keep the
directory. `DeleteRuntime` stops it and removes the directory.

### 3. Only fixed-workspace vendors respawn

`RuntimeVendor` is shared by `horsie connect` and `velos-runtime`. Respawning a
velos runtime means scheduling a new container with a fresh clone — silently
destroying work, which is the thing the current terminal behaviour exists to
prevent.

So §2's behaviour sits behind a builder flag, `with_respawnable_runtimes(bool)`,
defaulting to **false**. `horsie connect` sets it true. velos keeps today's
semantics exactly: hibernate declines, a get-miss is terminal.

The flag is deliberately separate from `supports_provisioning` rather than
derived from it. They answer different questions — "can you build a workspace?"
versus "is your runtime disposable?" — and a future vendor could differ on
either axis.

## Consequences

Killing the runtime on hibernate discards the runtime process's in-memory state
on every offload rather than only on a crash. `agents.json` covers the part that
matters (cwd and env overlay). Anything else a `horsie-runtime` holds in memory
— a running child process, an open file handle — does not survive, which was
already true across crashes and restarts.

`spec.json` persists the spec's `env`, which is where the server puts minted
secrets. For a fixed-workspace vendor that set is empty today: the plugin
manifest and token are gated on `supports_provisioning`
(`runtime_manager.rs:150`), and the GitHub token is only minted when there are
`git_checkout` provision steps, which such a vendor does not receive. It is not
empty by construction, though, and a persisted token replayed on respawn is
precisely issue #96. The file is 0600 on the user's own machine, alongside the
workspaces it grants access to. The real fix for #96 — the server re-supplying
`env` on get — remains open and this design does not block it.

A session deleted while the agent is offline never receives `DeleteRuntime`, so
its directory leaks. A few KB each. The reconciliation hook is `QueryRuntimes`
on reconnect, which is #92 item 4 and out of scope here.

## Testing

For §1: a provider returning `Transient` keeps the reconnect loop alive across
attempts; one returning `Dead` ends `run` with an error. Both fit the existing
millisecond-scale `with_backoff` harness in `runtime-vendor/tests`. On the CLI
side, `resolve_token` classifies a refused refresh as `Dead` and an unreachable
issuer as `Transient`.

For §2: create a runtime, drop the whole `RuntimeVendor`, build a new one over
the same state dir, and assert `GetRuntime` returns a working transport and that
a cwd set before the drop is still in effect after it. Assert the negative too:
the same sequence with `with_respawnable_runtimes(false)` still answers `Gone`.
`DeleteRuntime` removes the directory; `HibernateRuntime` does not.

## Out of scope

Issue #96 (server re-supplying runtime credentials on get), #92 item 4
(`QueryRuntimes` reconciliation), and the ops change putting `HORSIE_TOKEN` in
the velos vendor's environment (blossomstack/ops#79), which is independent of
both.
