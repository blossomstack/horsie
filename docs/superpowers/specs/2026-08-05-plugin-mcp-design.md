# MCP servers from plugins

#105's Phase 4, and the one it scheduled last because it needs "a new transport *and* a new
place for the MCP client to live". Both are real. This is how they land.

## The problem the issue states

`mcp-client` speaks Streamable HTTP and runs in the server process, "never inside the
sandbox". A plugin declares its servers in a top-level `<plugin>/.mcp.json` — not a
`plugin.json` field — and the common shape is `{"command": "npx", "args": [...]}`: a local
process that must run next to the workspace.

## Where it runs, and why that is the whole design

**Every plugin-declared MCP server is hosted by the runtime**, stdio and HTTP alike.

The stdio case forces it: a `npx …` server must run where the workspace is, and running it in
the server process would both be wrong (no workspace) and hand a plugin the ability to execute
commands on the server host. But the HTTP case belongs there too, and putting it anywhere else
would be a second path for no reason: a plugin's HTTP MCP server is as likely to sit on the
workspace's network as on the public internet, and the runtime is where horsie has decided
network position lives.

That gives one rule with no exceptions — *plugin MCP runs in the sandbox; admin-configured MCP
runs in the server* — rather than a transport-dependent split nobody could remember.

### Consequence: two new runtime messages

The runtime protocol is request/response over one socket, so hosting the client there means
exposing two operations:

- `McpDiscover { }` → every plugin-declared server's tools, namespaced. Called once per
  `provide()`, alongside the workspace scan.
- `McpInvoke { server, tool, arguments }` → one `tools/call`.

Connections are held per runtime connection, keyed by server name, and initialised lazily on
first use. Their lifetime is the runtime's, which is what makes a stdio child worth starting at
all — a process respawned per tool call would be slower than the call.

## Reconciling with the MCP horsie already has

The issue asks for this explicitly. The two stay **separate systems that share a protocol
client**:

| | admin-configured (`mcp_servers`) | plugin-declared (`.mcp.json`) |
| --- | --- | --- |
| where | server process | runtime (sandbox) |
| selected by | per-session picker | loading the plugin |
| auth | server-side OAuth | `env` in the declaration |
| transport | Streamable HTTP | stdio **and** HTTP |

**No OAuth for plugin servers**, and that is a decision rather than an omission: OAuth needs a
browser redirect back to *the server*, and a `.mcp.json` has nowhere to put a client
registration. What the format does have is `env`, which is how every published stdio server
passes its token. A plugin server needing interactive auth is one the user should add as an
admin server instead, where that flow exists.

Tools are namespaced `mcp__<server>__<tool>`, the same spelling the admin path uses, so
`allowed_tools` and hook matchers see one vocabulary.

## The declaration

`<plugin>/.mcp.json`, accepted in both shapes seen in the wild: servers wrapped in
`"mcpServers"`, or at the top level (which is what the official `example-plugin` ships).
Reading both costs one branch and is what the ecosystem actually contains.

```jsonc
{ "mcpServers": {
  "docs":   { "command": "npx", "args": ["-y", "@acme/docs-mcp"], "env": {"KEY": "…"} },
  "remote": { "type": "http", "url": "https://mcp.example.com/api" }
}}
```

`type` is optional and inferred: `command` present → stdio, `url` present → http. A
declaration with neither is skipped and named, exactly as a malformed hook declaration is.

`${CLAUDE_PLUGIN_ROOT}` is substituted in `command`, `args`, `env` values and `url` — a
plugin shipping its own server script has no other way to name it.

## Shape

- `horsie_support::plugin::mcp` — `read(plugin_root)` → `Vec<PluginMcpServer>`, both wrapper
  shapes, both transports, `${CLAUDE_PLUGIN_ROOT}` left *unsubstituted* (the root is the
  runtime's path, not the reader's).
- `mcp-client`: `StdioTransport` implementing the existing `McpTransport` trait — spawn, write
  newline-framed JSON-RPC to stdin, read from stdout, correlate by id. The trait is two
  methods; the protocol logic in `McpClient` is unchanged and shared.
- `runtime/src/mcp.rs` — the registry: name → live `McpClient`, initialised on first use,
  `discover()` and `invoke()`.
- `runtime-client` — `mcp_discover()` and `mcp_invoke()`.
- `server` — `PluginMcpToolbox`, composed in `provide()` from the discovery result, routing
  calls back over the same client. It sits beside the existing `McpToolbox` in the same
  composite, so the agent sees one tool list.

## Failure

A server that cannot start, or whose `initialize` fails, contributes **no tools** and logs —
it does not fail the turn. A plugin bringing a broken MCP server must not stop a session that
merely happens to load it, exactly as a plugin with an unreadable manifest contributes no
skills rather than blanking the library.

## Testing

- support: both wrapper shapes; type inference; a declaration with neither `command` nor
  `url`; `${CLAUDE_PLUGIN_ROOT}` preserved for the runtime to substitute.
- mcp-client: the stdio transport against a scripted child process — a request/response round
  trip, an interleaved notification, a child that dies mid-call.
- runtime: discovery namespaces tools per server; a server that fails to start contributes
  nothing and does not fail the scan.
- server: plugin tools appear in the toolbox alongside admin ones and route back over the
  runtime.

## Not in scope

OAuth for plugin-declared servers (above). MCP **resources and prompts**, which are #177 and
apply equally to both paths. Hot-reload of a `.mcp.json` mid-session: the declaration is read
at `provide()`, so a change lands on the next turn.
