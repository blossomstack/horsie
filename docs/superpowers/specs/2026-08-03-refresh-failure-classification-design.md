# Classifying refresh failures: only the server may declare a login finished

**Date:** 2026-08-03
**Scope:** `cli/src/auth.rs`. No server change.

## The failure

A `horsie connect` that had been running for hours died with:

```
vendor agent: lost the link to wss://horsie.lan.tthh.ca/api/vendor/connect; reconnecting in 1.0s
vendor agent: reconnected to wss://horsie.lan.tthh.ca/api/vendor/connect as "horsie-local"
vendor agent: lost the link to wss://horsie.lan.tthh.ca/api/vendor/connect; reconnecting in 1.0s
executor error: credential rejected: the stored login for https://horsie.lan.tthh.ca is no longer valid
```

and `~/.config/horsie/credentials.json` was left as `{"servers": {}}`.

The stored login was not, in fact, finished. Evidence from the deployment's
`auth_tokens` table at the time of the failure:

- chain `ec166fff`, issued 15:13:41Z; its access token expired 16:13:41Z.
- its **refresh token was live** — `revoked_at` null, `expires_at` 2026-11-01.

So the server never denied the refresh; it never saw one. The horsie container
was being recreated by the GitOps sweep in that window (the velos vendor's agent
token was last used at 17:05:37Z, reconnecting for the same reason), and Caddy
answered `POST /api/auth/refresh` with a gateway error on the CLI's behalf.

## Root cause

`resolve_token_outcome_with` classifies the refresh answer as:

```rust
match refreshed {
    Ok(pair) => { /* store and use */ }
    Err(_) => { creds.remove(server); TokenOutcome::Dead(...) }
}
```

`Err(_)` here is *any* non-2xx. `post_json` has already flattened the status
away, synthesizing `code: "http_502"` when the body is not the server's error
envelope — so a proxy's 502, a 429, and the server's own 500 are indistinguishable
from `access_denied` by the time the decision is made.

The transport-failure path was written carefully — a request that gets no answer
is `Transient`, with a test asserting the credential survives, precisely so a
server restart does not force a re-login. A reverse proxy that *answers* walks
straight past that guard.

The server's contract (`server/src/http/auth.rs:320`) is narrow: the only
"this credential is finished" answer is **HTTP 4xx with `code: "access_denied"`**
(`expired_token` is the device-code equivalent). `DeviceError::Internal` is a
500. Everything else on that path originates outside the application.

## Design

### 1. `post_json` keeps the status and admits an unparsable body

```rust
/// A non-2xx answer. `body` is `None` when the response was not the server's
/// error envelope — a proxy's HTML page, an empty body, a gateway error.
struct ApiFailure {
    status: u16,
    body: Option<ApiErrorBody>,
}

impl ApiFailure {
    fn code(&self) -> Option<&str>;
    /// The server's message when there is one, else the status line — so an
    /// operator reading a retry message can tell 502 from 429.
    fn message(&self) -> String;
}
```

`post_json` returns `Result<Result<T, ApiFailure>, CliError>`. The
`http_<status>` synthesis is dropped: a code the server never sent should not be
manufactured, because the whole point is to tell the server's words from
someone else's.

### 2. Only a terminal answer discards a login

```rust
/// The refresh endpoint's only "this credential is finished" answers. A 5xx, a
/// throttle, or a proxy page says nothing about the credential — and a login
/// destroyed on a guess costs a re-login the credential never needed.
fn is_terminal(&self) -> bool {
    (400..500).contains(&self.status)
        && matches!(self.code(), Some("access_denied" | "expired_token"))
}
```

- terminal → remove the credential, return `Dead`. Unchanged behaviour, now
  correctly gated.
- anything else → `Transient`, credential untouched, message naming the status.

Requiring **both** the 4xx and a parsed known code is deliberate. A proxy can
emit a 404 (bad route) or 403 (WAF, IP rule) with an HTML body; status alone
would let that destroy a working login. The cost is that a future server error
code would be retried rather than stopping the agent — visible in the log every
attempt, and recoverable, which is the direction to err in.

`RuntimeVendor::run` already retries `Transient` indefinitely with backoff to
30s and stops only on `Dead`, so no change is needed there: a restart window is
now ridden out instead of ending the process.

### 3. The device-flow poll stops aborting on a hiccup

`login`'s poll loop matches on the error code and falls to a `_` arm that
returns `CliError::Server`, ending the login. A gateway error during the
ten-minute approval window therefore kills a login the user may already have
approved in the browser.

The decision moves into a `poll_step(&ApiFailure) -> PollStep` free function
(`KeepPolling` / `SlowDown` / `Denied` / `Expired`), so it can be tested without
`login`'s side effects — the command writes to the real `credentials_path()`,
which a test cannot redirect without setting environment variables.

A failure that is not one of the flow's own codes keeps polling until the device
code's own deadline, which already bounds the loop. `access_denied` and an
expired code still end it.

## Testing

All in `cli/src/auth.rs`'s existing test module. The refresh cases drive a real
`resolve_token_outcome_with` against a stub issuer: a one-shot
`tokio::net::TcpListener` on `127.0.0.1:0` that reads the request and writes a
canned HTTP response — no new dependency, and hermetic. The poll cases call
`poll_step` directly.

| Case | Expected |
| --- | --- |
| refresh: 502 with an HTML body | `Transient`; credential still in the file |
| refresh: 500 with a JSON envelope | `Transient`; credential still in the file |
| refresh: 403 with an HTML body | `Transient`; credential still in the file |
| refresh: 400 `access_denied` | `Dead`; credential removed |
| poll: 502, 429, 500 `internal` | `KeepPolling` |
| poll: the four flow codes | `KeepPolling` / `SlowDown` / `Denied` / `Expired` |

The existing `an_unreachable_issuer_is_transient_and_keeps_the_credential` stays
as-is; it covers the path that already worked.

## Out of scope

- **Proactive refresh on a timer.** Refresh stays lazy — resolved before every
  dial. Worth revisiting: reconnects cluster around server restarts, which is
  exactly when the issuer is least reachable, so refreshing at ~50% of the
  access token's TTL while the link is healthy would mean a reconnect usually
  has a fresh token already in hand.
- **Parking instead of exiting on a genuinely dead credential.** `horsie connect`
  still exits and must be restarted after `horsie auth login`.
- Any server-side change. The contract is already right.
