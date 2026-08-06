# Auth for plugin MCP servers

The other half of [plugin MCP](2026-08-05-plugin-mcp-design.md). That design hosts every
plugin-declared server in the runtime and stops at a static `Authorization` header, ruling
OAuth out because "OAuth needs a redirect back to *the server*, and a `.mcp.json` has nowhere
to record a client registration."

Both halves of that sentence are true, and neither is about where the MCP *client* runs. Where
the credential lives and where the client lives are separable, and separating them is the whole
of this design.

## Placement is not a setting

Every plugin-declared server keeps running in the runtime, stdio and HTTP alike. The earlier
rule stands unchanged and gains no exception, no per-server override, and no plugin-author hint.

The only override anyone would have asked for is "move this one to the server so it can do
OAuth", and that reason disappears below. The one it must never grant is the stdio case:
running a marketplace plugin's `npx …` on the server host is arbitrary code execution on a
machine every user shares, which is not a checkbox. A setting whose safe position is the only
position is not a setting.

## The seam is already there

`HttpTransport` takes a [`BearerProvider`] — `bearer(force: bool)`, where `force` is the
transport's own signal after a `401`. The server-side path fills it with
`McpServerBearerProvider`, which resolves from the store, refreshes near expiry, and refreshes
unconditionally when forced. Plugin MCP fills it with `StaticHeaders`, a stub that reads one
header.

So this is not a new mechanism. It is the same trait, filled in on the runtime side by a
provider that asks the server.

## Where the credential comes from

**The server sends it with the request.** `McpDiscover` and `McpInvoke` carry the tokens for
the servers they touch, resolved server-side immediately before the send by the same
`resolve_bearer` the admin path uses. A `401` the runtime still gets comes back as a typed
outcome, and `PluginMcpToolbox::execute` retries the call once with a forced-fresh token —
transparent to the agent, one extra round trip, and the credential never outlives the call
inside the sandbox.

| | pushed with the request | runtime calls back for it | injected at provisioning |
| --- | --- | --- | --- |
| new protocol direction | none | runtime → server requests | none |
| refresh | per call, server-side | per call, server-side | never |
| lifetime in the sandbox | the call | the call | the runtime's |

The callback shape is cleaner in the abstract and costs a reverse request path the runtime
protocol does not have — the same reverse-dial surface [#191] names as the thing blocking the
sandbox providers. Not a door to open for this. Injection at provisioning has no refresh and
leaves a long-lived token beside plugin-controlled child processes.

## Deciding that a server needs auth

Nothing is asked and nothing is guessed. RFC 9728 is the mechanism:

1. The runtime `initialize`s with whatever the declaration carries, which is usually nothing.
2. A protected server answers `401` with a `WWW-Authenticate` naming its protected-resource
   metadata. That header *is* the signal.
3. The runtime reports `needs_auth { server, resource_metadata }` in the discovery result
   rather than a failure string, and contributes no tools this turn.
4. The server discovers the authorization server and registers a client dynamically — both
   already in [`oauth.rs`] — and offers **Connect**, whose URL `connect_oauth` already builds.
5. The user consents once. Every later request carries a token that refreshes itself.

Step 5 is the only human step, and it is irreducible: no design authorizes an application on a
user's behalf without the user authorizing it.

## What a declaration still carries

`env` and `headers` remain exactly what they are for a server that wants a static token, and
they are read as written — every declared header, at its declared value. `StaticHeaders`
today forwards only `authorization` and rewrites it as `Bearer`, so `X-API-Key` vanishes and
`Authorization: token …` becomes `Bearer token …`. That is a bug in the stub, not a property
of the format.

Static and negotiated compose in the obvious order: declared headers always go on the request,
and the bearer, when there is one, comes from the server.

## Shape

- `mcp-client`: `McpError::Unauthorized { www_authenticate }`, so a `401` is distinguishable
  from a transport failure. `HttpTransport::new_with_headers` for arbitrary static headers, and
  the `401` retry keeps its current behaviour.
- `models/fluorite/runtime.fl`: `McpDiscoverResponse.failures` becomes a union —
  `Unreachable { server, reason }` or `NeedsAuth { server, resource_metadata }`. A string is
  enough to log and useless to act on. `McpDiscoverRequest` and `McpInvokeRequest` gain
  `credentials: Vec<McpCredential>`.
- `runtime/src/mcp.rs`: `StaticHeaders` honours every declared header; a supplied bearer rides
  beside them; a `401` on `initialize` becomes `NeedsAuth` rather than a failure line.
- `server/src/mcp`: plugin servers get rows in the same store as admin ones, distinguished by
  origin so the picker does not offer them and `delete` cannot orphan a plugin. `resolve_bearer`
  is reused verbatim.
- `workflow/src/mcp_toolbox.rs`: `PluginMcpToolbox` resolves credentials before each invoke and
  retries once on `NeedsAuth`.
- Web: a plugin server awaiting consent appears in Settings → MCP as a **Connect** row it does
  not otherwise own — the plugin declared it, so it cannot be edited or removed there.

## Failure

Unchanged and still never fatal. A server awaiting consent contributes no tools and is not an
error: the session runs, the agent simply does not see those tools. A server whose token
refresh fails is `NeedsAuth` again, which puts the Connect row back rather than failing a turn.
Revoking a plugin, or the plugin going away, deletes its rows and its stored tokens.

## Testing

- `mcp-client`: a `401` carrying `WWW-Authenticate` becomes `Unauthorized` with the header
  preserved; arbitrary static headers reach the request unaltered, including a non-`Bearer`
  `Authorization`.
- runtime: a declared server answering `401` yields `NeedsAuth` and no tools; a supplied bearer
  reaches the server alongside declared headers.
- server: a plugin server's first discovery produces a Connect row; after a token is stored,
  discovery produces tools; a token forced-refreshed on `401` retries the invoke once and only
  once.
- The composition test [#221] owes: a plugin server and an admin server of the same name do not
  shadow each other, whichever is authenticated.

## Not in scope

Auth for stdio servers beyond `env` — a local process has no redirect to receive and no 401 to
answer. MCP resources and prompts, still [#177]. Letting a plugin *reference* an admin server
by name instead of declaring one: a smaller feature that solves a different problem, and worth
its own issue.

[`BearerProvider`]: ../../../mcp-client/src/transport.rs
[`oauth.rs`]: ../../../server/src/mcp/oauth.rs
[#177]: https://github.com/blossomstack/horsie/issues/177
[#191]: https://github.com/blossomstack/horsie/issues/191
[#221]: https://github.com/blossomstack/horsie/pull/221
