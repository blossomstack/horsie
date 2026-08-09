# The runtime credential channel

**Goal:** make the dial token the *only* credential a runtime carries in its environment, and mint every other secret on demand against it.

## The problem

Three secrets ride a runtime's environment today, and two of them expire:

| Env var | Minted by | Lifetime |
|---|---|---|
| `HORSIE_CONNECT_TOKEN` | the vendor | never expires |
| `GITHUB_TOKEN` | GitHub (App installation token) | ~1 hour |
| `HORSIE_PLUGINS_TOKEN` | the server (HS256 JWT) | 1 hour |

`RuntimeManager::runtime_spec` re-mints all of them on every `create` *and* every
`get`, with a comment saying a stale one is worse than none. But both in-server
vendors discard the spec on acquisition — `fly.rs` and `velos.rs` each say
`let _ = spec;` — so the environment is frozen at create. A runtime that has
been up for an hour is holding a dead GitHub token, and a Fly machine resumed
from hibernation comes back with the environment it was born with.

There is no way for the server to hand a live runtime a new secret: the runtime
protocol has no message for it, and no substrate can rewrite a running machine's
environment.

## Why the dial token is different, and why that is the lever

The dial token is an **identity** credential in a closed loop — horsie mints it,
horsie verifies it, and it authorises exactly one runtime id. The other two are
**delegated authorisation** to a resource, with lifetimes horsie does not
control.

That makes the dial token a *root* and the others *leaves*. A non-expiring root
that fetches fresh leaves is coherent. Two expiring leaves side by side with no
renewal path is not.

So: keep the root in the environment, and fetch every leaf on demand.

## The blocker that has to be cleared first

The dial token is not currently server-verifiable for every runtime.

- **Fly and velos** mint with the account's `runtime_dial_secret`, read from the
  `settings` table. The server can verify these.
- **`horsie connect`** mints with `new_dial_secret()` — a random value generated
  in-process at startup (`crates/cli/src/connect.rs`), which the server has
  never seen. Its runtimes dial the *vendor's* unix socket, verified by the
  vendor itself, and never touch `/api/runtime/connect`.

Two disjoint trust domains sharing one token format. Any design that treats the
dial token as a server-facing credential works for cloud runtimes and silently
fails for every self-hosted one.

## Decisions

### D1 — The server mints every dial token

`RuntimeSpec.env` already exists for exactly this: its doc comment reads
"resolved secrets and handles only the server can mint". The dial token joins
the GitHub token and the plugin manifest there.

- `RuntimeManager::runtime_spec` mints with the account's `dial_secret` and
  pushes `HORSIE_CONNECT_TOKEN`.
- Fly and velos stop minting; they already copy `spec.env` into the machine
  environment, so they simply stop adding their own.
- `RuntimeVendorClient` stops minting and forwards what the spec gave it.
- `new_dial_secret()` and `with_dial_secret()` are deleted.

No new round trip and no new endpoint: the server already sends a spec on
`CreateRuntime` and `GetRuntime`.

The frozen-environment problem does not apply to this one value, because the
dial token never expires. That is the whole reason the root/leaf split works.

### D2 — The `horsie connect` listener authenticates by issued-token lookup

The vendor can no longer HMAC-verify, because it no longer holds the secret. It
does not need to: it is the party that handed the token out, so it records
`token -> runtime_id` when it provisions and consults that map on dial-back.

This is strictly stronger than the HMAC check it replaces. A token the vendor
never issued is not merely unsigned — it is unknown.

### D3 — `HORSIE_SERVER_URL` replaces `HORSIE_PLUGINS_BASE`

The runtime needs one address to reach the server at, and it now needs it for
two things rather than one. `HORSIE_PLUGINS_BASE` is renamed and generalised.

- `horsie connect` supplies its own `--server` value, as it does today.
- Fly and velos derive it from `callback_url` (`ws://` → `http://`, `wss://` →
  `https://`, minus the `/api/runtime/connect` path).

**This fixes a live bug.** No in-server vendor has ever set
`HORSIE_PLUGINS_BASE`, so `provision_plugins()` returns `None` immediately on
every Fly and velos runtime: plugin bundles have never worked on the cloud
vendors at all. Deriving the URL from `callback_url` is required for the GitHub
half of this work regardless, and repairs bundles as a side effect.

### D4 — Artifacts authenticate with the dial token

`GET /api/plugin-artifacts/<hash>.zip` takes the dial token as its bearer. The
server verifies it the same two-phase way `/api/runtime/connect` does, then
checks the hash is one the *account* has installed (`PluginStore::list()` is
user-scoped).

Deleted: `plugins/token.rs`, `ENV_PLUGINS_TOKEN`, `Shared::artifact_secret`,
`ArtifactStore::sign_token`/`verify_token`, the `HORSIE_ARTIFACT_SECRET`
environment variable, and `PluginProvisioner::mint_token`.

**The scope changes from per-session to per-account, deliberately.** The old JWT
named an exact hash set, but that protected nothing real: the same principal can
select any of its own bundles into a new session at will. Meanwhile the endpoint
had *no account check whatsoever* — `artifact_secret` is deployment-global and
the route runs ahead of the auth layer, so any account's token fetched any
account's artifact. Per-account is a strict improvement on the boundary that
actually matters.

### D5 — GitHub moves to a git credential helper

New endpoint, dial-token authenticated:

```
GET /api/runtime/github-credential?host=<host>&path=<owner/repo>
```

The server verifies the token, resolves `runtime_id` to its session (the runtime
id *is* the session id), reads that session's `git_checkout` provision steps,
and refuses unless the requested repo is among them. On success it calls the
existing `mint_token_for` and returns a token scoped to that one repo.

The runtime side is a new `horsie-runtime git-credential` subcommand
implementing git's credential protocol. At startup — in sync `main()`, before
the tokio runtime is built, which is the only safe window for `set_var` — the
runtime sets:

```
GIT_CONFIG_COUNT=2
GIT_CONFIG_KEY_0=credential.https://github.com.helper
GIT_CONFIG_VALUE_0=<absolute path to self> git-credential
GIT_CONFIG_KEY_1=credential.https://github.com.useHttpPath
GIT_CONFIG_VALUE_1=true
```

`useHttpPath` is not optional: without it git does not pass `path=` to the
helper, and the server cannot scope the token to a repo.

Every descendant inherits this, so both the provision-step clone and the agent's
own `bash` tool calls get credentials with no further plumbing. `steps.rs` drops
its `GIT_CONFIG_*`/`http.extraHeader` code and its `github_token` parameter
entirely.

Consequences:
- The token is minted at the moment of use, so a one-hour TTL never matters and
  "refresh" stops being a thing that exists.
- `git push` starts working. It cannot today: the clone deliberately leaves no
  credential in `.git/config`.
- `GITHUB_TOKEN` leaves the environment.

The helper exits 0 with no output when it cannot mint, which is what git expects
for "no credentials available" — so a public-repo clone still works on a
deployment with no GitHub connection.

## What this does not fix, stated plainly

The agent's `bash` tool inherits the runtime's environment (`bash.rs` does no
`env_clear`), so the agent can read `$HORSIE_CONNECT_TOKEN` and call the
credential endpoint itself. Moving GitHub behind a helper does not change that,
and pretending otherwise would be dishonest.

The real boundary is the runtime, not the process tree: anything the agent's
bash can do, the agent can do. What this work buys is narrower and still worth
having — no *expiring* secret in the environment, no long-lived GitHub token
sitting in it, per-operation minting scoped to the session's own repos, and
revocation that takes effect immediately because the server is consulted on
every mint rather than an hour ago.

## Out of scope

- Rotating `runtime_dial_secret`. There is no rotation path today, and building
  one is a separate piece of work — see the note below.
- Relaying artifact bytes over the vendor WS link. HTTP with a verifiable bearer
  is enough, and keeps `HORSIE_SERVER_URL` as the one address the runtime needs.
- Per-runtime revocation. Now *possible* (the server is consulted on every
  mint), but it needs a UI and a policy, and neither belongs here.
