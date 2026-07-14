# MCP mid-turn token refresh & 401 retry

- **Date:** 2026-07-12
- **Status:** Design — approved
- **Branch (planned):** `mcp-token-retry`

## Summary

Make MCP tool calls survive a bearer token that expires **mid-turn**. Today
`McpService::build_toolbox` resolves a token once at agent spawn and hands
`HttpTransport` a *static* `Option<String>`; a long turn (many `tools/call` over
minutes) whose token dies partway through gets an HTTP 401 that surfaces as a
failed tool call with no recovery. This adds a pluggable **bearer provider** to
the transport and a **single forced-refresh retry on 401**, so OAuth and
`github_app` servers self-heal within a turn. This is the seam the MCP spec
anticipated ("auth via a pluggable Authorization header provider so the token
can be refreshed").

## Goals

- A `tools/call` (or connect) that returns 401 triggers **one** forced token
  refresh and a retry; on success the tool call proceeds normally.
- Refresh logic (OAuth refresh-token exchange; GitHub user-token re-mint) is
  reused, now with a `force` path that refreshes even when the token is not yet
  within the near-expiry window.
- No behavior change when tokens are valid: exactly one token is resolved per
  turn (cached in the provider), same as today.

## Non-goals

- **Tool-list caching** — per-turn `connect` + `tools/list` stays (a deliberate
  freshness choice in the original spec).
- Retrying more than once, or on statuses other than **401** (a second 401, or a
  provider that can't produce a new token, returns the error as today).
- Retrying `notify` (notifications carry no response; the initialized
  notification uses the fresh spawn-time token).
- Any change to `github_app`'s acceptance by the remote GitHub MCP server (the
  spec's untested primary risk) — out of scope.

## Current shape (verified)

- `mcp-client/src/transport.rs`: `HttpTransport::new(endpoint, bearer: Option<String>)`
  holds a static bearer, injects it in `build()`, and in `request()` turns any
  non-2xx into `McpError::Transport("http {status}: {text}")`. **Only caller of
  `HttpTransport::new`:** `server/src/mcp/service.rs:275` (`build_toolbox`).
- `server/src/mcp/service.rs`: `bearer_for(row)` (one caller — `build_toolbox`)
  resolves the token per auth kind: `none`→None, `bearer`→stored,
  `github_app`→`github.user_token()`, `oauth`→stored/refresh-if-near-expiry.
- `server/src/github/service.rs:196`: `user_token()` refreshes only when
  `needs_refresh`. **Only caller:** the `github_app` arm of `bearer_for`.

## Design

### 1. `mcp-client` — `BearerProvider` + transport retry

New trait (in `transport.rs`):

```rust
/// Supplies the `Authorization: Bearer` for an MCP connection, refreshably.
/// `force = true` (issued after a 401) must attempt a fresh token, bypassing any
/// cache. `Ok(None)` means "no auth" (public server).
#[async_trait]
pub trait BearerProvider: Send + Sync {
    async fn bearer(&self, force: bool) -> Result<Option<String>, McpError>;
}
```

`HttpTransport` holds `auth: Arc<dyn BearerProvider>` instead of
`bearer: Option<String>`. `new(endpoint: String, auth: Arc<dyn BearerProvider>)`.

`request()` flow:
1. `token = auth.bearer(false)` → build + send.
2. If the response status is **401**: `fresh = auth.bearer(true)`. If
   `fresh != token && fresh.is_some()`, rebuild + resend **once** with `fresh`
   and use that response; otherwise fall through with the original 401.
3. Parse the (final) response as today (JSON or SSE body → `extract_result`).

`notify()` is unchanged (no retry). Session-id capture happens after each send.
The SSE-parser and error-mapping helpers are untouched.

**Tests:** a mock `BearerProvider` (returns `"old"` then `"new"` on force) against
a mock HTTP MCP server that 401s any request without `Authorization: Bearer new`
and 200s otherwise → assert the request succeeds and the provider saw a
`force=true` call; and a no-refresh provider (returns the same token on force)
→ the 401 propagates.

### 2. `server` — the provider implementation + `force` refresh

`bearer_for`'s logic moves into a `BearerProvider` impl. To let it read the row,
refresh, and persist without an `Arc<McpService>`, the provider owns cheap clones
of the collaborators (all already held by `McpService`):

```rust
struct McpServerBearerProvider {
    store: McpStore,            // #[derive(Clone)] — wraps a clonable SqlitePool
    oauth: McpOAuthClient,      // #[derive(Clone)] — wraps a clonable reqwest::Client
    github: Arc<GithubService>,
    server: String,
    cached: tokio::sync::Mutex<Option<Option<String>>>, // outer: fetched?; inner: token
}
```

- `bearer(false)`: returns the cached token if present; else resolves once and
  caches (→ one token per turn, matching today).
- `bearer(true)`: bypasses the cache, resolves with `force`, updates the cache.
- Resolution reuses the current per-auth-kind logic via a shared
  `resolve_bearer(&store, &oauth, &github, &row, force)`:
  - `none` → `None`; `bearer` → stored token (force is a no-op — static);
  - `github_app` → `github.user_token(force)`;
  - `oauth` → refresh when `force || needs_refresh(expires_at)` (force refreshes
    regardless of the near-expiry window), else the stored access token; on a
    refresh, persist via `save_oauth_tokens` and return the new token; if a
    refresh isn't possible (no refresh_token/meta/client_id), return the current
    token.

`McpStore` and `McpOAuthClient` gain `#[derive(Clone)]` (both wrap only clonable
handles). `build_toolbox` constructs the provider from `self.store.clone()`,
`self.oauth.clone()`, `self.github.clone()`, and `row.name`, then
`HttpTransport::new(row.url, provider)`. `McpService::bearer_for` is removed (its
sole caller was `build_toolbox`); `server_view`/`connect_oauth`/`callback` are
unaffected. Because `build_toolbox` backs both `test()` and `toolboxes_for`, the
smoke test and per-turn spawn both gain the retry.

**Tests:** `resolve_bearer` with `force = true` on an `oauth` row refreshes even
when `expires_at` is far in the future (mock AS token endpoint), persists the new
token, and returns it.

### 3. `GithubService::user_token(force: bool)`

Add a `force` parameter: when `true`, refresh via the stored `refresh_token`
regardless of `needs_refresh` (return the current token if no refresh token
exists — nothing more can be done). Update the single caller (the `github_app`
arm of `resolve_bearer`). Existing `needs_refresh`/refresh mechanics unchanged.

## Error handling

- A refresh failure inside `bearer(true)` propagates as
  `McpError::Transport(msg)` — the tool call fails with a clear message rather
  than a bare "http 401".
- A second 401 (post-refresh) returns the transport error as today (no loop).
- Static (`bearer`) and `none` servers: `force` yields the same token/None, so
  the transport does not retry — behavior is unchanged.

## Testing summary

- `mcp-client`: transport retries once on 401 with a refreshed token (mock
  provider + mock server); no retry when the token is unchanged.
- `server`: `resolve_bearer(force=true)` force-refreshes an OAuth token that is
  not near expiry; the existing OAuth connect/callback + store/service tests stay
  green after the `bearer_for`→provider refactor.
- Full gate (`fmt`/`clippy`/`test`/`deny`, web unaffected) green; lands as one PR.

## Alternatives considered

- **Widen the spawn-time refresh margin** (refresh if < ~10 min left): a one-line
  change, no transport work, but only shrinks the window — a turn longer than a
  freshly-refreshed token's lifetime, or a non-refreshable token, still fails.
  Rejected in favor of the real fix; the transport seam exists for it.
- **Retry at the `McpToolbox` layer**: the toolbox holds an `Arc<McpClient>` and
  can't reach the refresh logic without also carrying a provider — worse layering
  than putting the retry in the transport that already sees the 401.
</content>
