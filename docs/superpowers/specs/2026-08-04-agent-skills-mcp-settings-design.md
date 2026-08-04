# Agent presets: no runtime, always skills and MCP

**Date:** 2026-08-04
**Status:** approved

## The problem

An agent preset is a saved session configuration you invoke with a message. Its
form (`/agents/:name/edit`) renders the same pickers as the session config bar,
through `useConfigPickers`. Three of those pickers — Repos, Skills, MCP — sit
behind `if (draft.provisions)`, where `provisions` is the selected runtime
vendor's `supportsProvisioning` capability.

`horsie connect` announces `supports_provisioning: false`. It is a fixed,
user-owned directory: no repo checkout, no bundle install. So on the most common
self-hosted setup, an agent preset offers no Skills picker and no MCP picker at
all.

That single bit is doing three different jobs, and it is only right for one of
them:

| Channel | Reaches a non-provisioning vendor today? |
| --- | --- |
| MCP | **Yes.** `McpToolbox` is composed server-side and never touches the runtime. The gate is a UI accident. |
| Skills | **No** — but only because `server/src/runtime_manager.rs:150` skips injecting the bundle manifest unless `supports_provisioning`. `cli/src/connect.rs:306` already calls `.with_bundles(…)`. |
| Repos | **No, genuinely.** A user-owned directory gets no checkout. |

Separately, a preset can name a runtime vendor. That pin is invisible once the
vendor disconnects and fatal at invoke: `server/src/http/agents.rs:114` rejects
the invocation with *"runtime vendor 'X' is not connected"*. Routines invoke
presets on a schedule, so the failure surfaces as a silently broken routine.

## The change

Remove Runtime from the agent preset. Show Skills and MCP on it unconditionally,
and make the Skills selection actually take effect on every vendor.

### 1. The agent page

`useConfigPickers` takes a surface: `useConfigPickers(draft, "agent" | "session")`.

- **Runtime** — session only.
- **MCP** — both, ungated.
- **Skills** — both, ungated.
- **Repos** — both, still gated on `provisions`.

On the agent surface `provisions` now resolves from the server's default vendor,
which `useAgentDraft` already computes as `vendor || settings?.defaultVendor`.
Dropping the picker removes the first operand and nothing else; the Repos gate
keeps working with no new logic.

The session config bar is unchanged. Runtime belongs there, where you choose
where this session actually runs.

### 2. `vendor` leaves the preset

Removed from `AgentView` and `AgentPresetInput` in `models/fluorite/agents.fl`,
regenerated into Rust and TypeScript. `AgentRow` and the `COLS` list in
`server/src/agents/store.rs` lose the column; migration `0018` drops it, in both
`server/migrations/sqlite/` and `server/migrations/postgres/`.

`invoke_agent` reads `config_store.default_vendor()` directly instead of
`agent.vendor.unwrap_or_else(…)`. The connectivity check stays — it is still the
right error when nothing is connected.

`cli/src/agent.rs` loses the vendor column from `list` and the vendor line from
`get`.

Nothing but the web form and raw API `PUT`s ever set the field, so this removes
an override with no callers rather than a feature in use.

### 3. Skills reach every vendor

Delete `&& vendor.capabilities().supports_provisioning` at
`server/src/runtime_manager.rs:150`. The bundle manifest then reaches any vendor
whose agent wired `with_bundles`, which `horsie connect` does.

The resulting contract is the one `runtime/src/main.rs:229` already implements,
unchanged:

> A session that selects bundles gets exactly those. A session that selects none
> falls back to the host `--plugins-dir` library.

Second-order effect, and the only live-behaviour change in this spec: on a
`horsie connect` vendor, selecting a bundle now **replaces** the host library for
that session instead of leaving it in place. That is the existing precedence
rather than something introduced here, and it is the right reading of an explicit
selection. It gets a line in `docs/guide/`.

This does not touch velos, which already provisions and already receives
manifests. The velos artifact-base bug is #99 and is independent.

### 4. The silent wipe

`buildAgentInput` drops `repos`, `plugins` and `mcpServers` whenever `provisions`
is false. `PUT /api/agents/:name` is a full replace, so today opening any preset
while the default vendor does not provision and pressing Save wipes all three
fields.

`plugins` and `mcpServers` stop being gated — they are valid everywhere now.
`repos` preserves whatever the preset already held instead of emptying it.

## Testing

- `useAgentDraft.test.tsx:127` inverts: "drops repos/skills/mcp when the vendor
  cannot provision" becomes "keeps skills and mcp, preserves repos". The vendor
  assertions at :84, :102 and :117 go.
- A new unit test: the agent surface yields no `runtime` picker, the session
  surface still does.
- `n-agents.spec.ts` asserts `config-skills` and `config-mcp` are visible on the
  agent form. The e2e harness runs the non-provisioning `e2e` vendor, which makes
  this the exact regression guard for the reported problem.
- `j-new-session.spec.ts:15` narrows its comment and assertions to repos only.
- `server/src/agents/store.rs` round-trip tests lose vendor.
- A `runtime_manager` test that a non-provisioning vendor receives the plugin
  manifest env.

## Out of scope

Three real gaps between "horsie supports MCP servers" and "horsie supports MCP",
none of them this change:

- **stdio/local MCP servers** — tracked in #105 Phase 4.
- **MCP servers declared by a plugin's `.mcp.json`** — tracked in #105 Phase 4.
- **MCP resources and prompts** — tracked in #177.

There is no entity called "an MCP" distinct from an MCP server. MCP is the
protocol, an MCP server is what is on the other end, and horsie is the client.
The three items above are the axes horsie does not yet cover.
