# Plugin support (#105) — handoff

Written 2026-08-04. Everything needed to resume without re-deriving it.

## The goal, and how it grew

It started with one bug: `horsie plugin install https://github.com/pbakaus/impeccable`
failed with `does not expose any SKILL.md`. Investigating showed the CLI gate was
a filesystem heuristic that never read `.claude-plugin/plugin.json`, while the
runtime's loader *was* manifest-aware — two disagreeing notions of "is this a
plugin".

Pulling that thread found more: horsie parsed the plugin manifest in **three**
divergent places, ignored marketplaces entirely, and ran **one** of Claude
Code's 31 hook events. Issue **#105** tracks closing the gap in five phases.

## Where things stand

| Phase | State |
| --- | --- |
| 0 PR1 — shared reader, manifest + marketplace resolution, symlink install layout | **merged (#110)** |
| 0 PR2 — marketplace registry, install by name, external sources | **merged (#113)** |
| 0 PR3 — server marketplaces + web UI | not started |
| 1 PR1 — hook dispatch, `PreToolUse` / `PostToolUse` | **open (#140)**, CI green |
| 1 PR2 — `Stop` + unsupported-event gates | **open (#141)**, stacked on #140 |
| 2 — agents · 3 — commands · 4 — MCP | not started |

**The original bug is fixed and shipped.** `plugin install impeccable` works.

### Branches and worktrees

- `feat/plugin-hooks` → `.horsie/worktrees/plugin-hooks` → PR #140
- `feat/plugin-hooks-events` → `.horsie/worktrees/plugin-hooks-2` → PR #141

**#141 predates the #140 rework** and still assumes the old server-side
architecture. It will need reworking or rebuilding once #140 lands — do not
merge it as-is.

**CI does not run on #141** and that is not a failure: `ci.yml` triggers only on
PRs targeting `main`. It deliberately includes the `edited` event so a stacked
PR picks up CI when its parent merges and GitHub retargets it. Merge #140 and
#141's run starts by itself.

## Uncommitted work in the `plugin-hooks` worktree

`models/fluorite/runtime.fl`, `runtime/src/hooks.rs`, `runtime/src/main.rs`:
adds `tool_call_id` to `HookRecord` and threads it through. Workspace builds;
the 12 runtime hook tests pass. **Not committed.** It is a prerequisite for the
web UI work below — without a join key a record cannot be attached to the tool
call it describes.

## Architecture, and the pivot

The first version of #140 ran tool hooks **server-side**: a `HookedToolbox`
decorator on the toolbox stack, a hook manifest fetched at session start, and
server-evaluated matchers round-tripping a `RunHook` message to the runtime.

That was wrong. Every runtime vendor's responsibility ends at materialising
`plugins_dir` (`runtime/src/main.rs` — velos fetches bundles, local passes
`--plugins-dir`, both converge on one directory), after which **the runtime is
the only component that can see plugin files**. Running the decision anywhere
else ships it away from the data.

Tool hooks now run **inside the runtime, inline with the tool call they guard**.
That deleted the manifest message, the `RunHook` message, server-side matcher
evaluation, and `HookedToolbox` entirely — and fixed three things the old design
had accepted as costs:

- **No extra round-trip.** The manifest existed only to let the server skip a WS
  hop per tool call. Inline, there is nothing to skip.
- **No version negotiation.** The manifest doubled as a protocol-version probe.
  A runtime that predates hooks simply runs none.
- **Cancellation works.** A `RunHook` round-trip sat outside `CancelCall`, so a
  30s hook ignored a user pressing Stop. Inline, it is interrupted by the same
  cancel that interrupts the tool.

Most of the control protocol then needed **no new wire types**: a denial is
`ToolResult::Err`, which the agent loop already turns into an `is_error` tool
result the model reads; `updatedInput` is applied before dispatch;
`updatedToolOutput` is what the runtime returns. `agentcore` is untouched.

Turn and session events (`Stop`, `UserPromptSubmit`, `SessionEnd`) **cannot**
move runtime-side — the runtime has no idea a turn ended. Those stay
server-initiated, as `SessionStart` already is.

## Locked decisions — do not relitigate without new evidence

- **Fail closed on `PreToolUse` only.** A hook that times out, fails to spawn, or
  exits non-zero-and-not-2 denies the tool: a guard that cannot run is not a
  guard. Every other event runs after the fact, so it cannot block.
- **`permissionDecision: "ask"` / `"defer"` are treated as allow**, and logged.
  horsie has no permission prompt and runs unattended sessions. Note this pulls
  opposite to fail-closed — a crash denies, an explicit ask allows. Intentional:
  a crash is an outage, `ask` is a signal horsie cannot act on.
- **Unsupported hook events fail loudly** rather than silently no-op (gates land
  in #141). No other harness does this; horsie is deliberately stricter.
- **Every hook that runs is recorded, including no-ops.** "A guard ran and
  allowed this" is part of the audit trail.
- **`sha` in marketplace entries is ignored** — it digests a packaging horsie
  does not reproduce, so honouring it would claim a verification we do not do.
  `ref`/`commit` carry the pinning.
- **`sources/<key>` is keyed by a hash of `(url, ref)`**, not plugin name, so a
  marketplace declaring several plugins from its own repo clones once.
- **Removing a marketplace does not uninstall plugins from it.** Dropping a
  source is not dropping the software.

## Ecosystem facts that set the scope

Do not re-research these; they were measured, not assumed.

- **Skills are a real cross-harness standard** ([agentskills.io](https://agentskills.io/specification)):
  `skill-name/SKILL.md` plus frontmatter. It defines **nothing** about packaging,
  plugins or registries.
- **Plugins and marketplaces are Claude Code's**, with Grok Build the only
  adopter (it reads Claude's format verbatim). Gemini CLI "extensions" are a
  separate, incompatible concept. There is no neutral plugin standard to adopt.
- **Hook vocabularies differ per harness**: Claude 31 events, OpenCode ~28
  (`tool.execute.before`, JS plugin code), Cursor ~21 (camelCase,
  `.cursor/hooks.json`), Codex 11 (Claude's exact PascalCase names). impeccable
  ships **four separate hook manifests**, one per vocabulary.
- **Real-world demand is tiny.** Across every plugin in the official marketplace,
  only six events are declared anywhere: `Stop` (3), `SessionStart` (3),
  `UserPromptSubmit` (2), `PostToolUse` (2), `PreToolUse` (1),
  `UserPromptExpansion` (1). This is why the supported set is small — building
  15 "because the seam exists" was rejected as speculative.
- **Of the marketplace's 53 path-source plugins, only 17 ship skills.** The other
  36 are agents/commands/MCP-only and stay uninstallable until Phases 2–4.
- **223 of the marketplace's 276 entries point at external repos**, which is why
  PR2's resolver had to handle all four `source` forms.

## Traps

- **Tool-name aliasing is load-bearing.** Every matcher in the wild names
  Claude's tools (`Bash`, `Edit|Write|MultiEdit|NotebookEdit`); horsie's are
  snake_case and match none. `support/src/plugin/hooks.rs::claude_aliases` is
  what makes any hook fire. Break it and the feature is silently inert.
- **Two name spaces in the runtime.** The wire `ToolCall` union is tagged
  PascalCase; the LLM, matchers and users see snake_case. `runtime::hooks::tool_name`
  bridges them with an exhaustive match, so adding a tool fails to compile until
  it is named.
- **Adding a variant to `RuntimeInbound/OutboundMessage` breaks ~6 exhaustive
  matches**, including `runtime-client/src/testkit.rs` and
  `server/src/runtime_vendor/fake.rs`. The workspace lints deny wildcard arms.
- **`cargo test -p horsie-runtime-client` alone fails** on a missing
  `horsie_agentcore::testkit` — a feature-unification artifact, not a
  regression. CI runs the workspace; test that way.
- **Compare like with like when diagnosing.** A clippy failure was twice
  misdiagnosed as "my change caused it" by running `-p <crate>` on the base and
  workspace-wide on the branch. It was a stale base; main had already fixed it.
- **Scripted edits silently match nothing** when an anchor has moved. Three
  insertions no-oped after a rebase and only the compiler caught them. Verify
  every scripted insertion.
- **Main moves fast** — 36 commits landed under one branch during this work,
  including changes that invalidated two spec claims (see below). Rebase often.
- **Playwright is flaky.** Two different tests failed once each with the same
  `tool-call-output` locator timing out, and both passed on a bare re-run. Check
  whether sibling tool-call tests passed before suspecting a regression.

## Spec claims that went stale mid-build

Both corrected in the spec and guide, but worth knowing they changed:

- **#115 inverted the sandbox default.** `horsie connect` now sandboxes runtimes
  by default; the flag went from opt-in `--sandbox` to opt-out `--no-sandbox`.
  Earlier docs said hooks run unsandboxed by default — no longer true.
- **#116 added session subagents.** `SubagentStart`/`SubagentStop` now have a
  real seam rather than a pending one.

## Immediate next steps

1. **Commit the `tool_call_id` work** in the `plugin-hooks` worktree.
2. **Web UI for hook records.** The user's requirement: hook results flow to the
   server, persist, and show **in the session transcript as messages** — not in
   `SessionDetail`. Records already persist as `SessionDomainEvent::HookRan`.
   What remains is display:
   - The transcript is built from **agent LLM messages** (`/history` →
     `Vec<Message>`, streamed as `AgentFrame::Appended`). `sse.rs` is explicit:
     *"Nothing here reads a journal."* Session-journal events do not appear there
     automatically.
   - **Do not** inject a synthetic `Message` into agent history — it would enter
     the model's context and cost tokens on every call.
   - Plan: a session-journal read path returning records keyed by
     `tool_call_id`, which the client merges into the transcript, **plus** an
     ephemeral `AgentFrame` variant so records appear live during a turn.
     Ephemeral alone is not enough — like `Delta` and `ToolStart` it is never
     replayed, so a reload would show the tool call with no hook history.
3. **Merge #140**, which unblocks CI on #141.
4. **Rework #141** onto the runtime-side architecture.
5. Phases 0 PR3, 2, 3, 4 each need their own brainstorm → spec → plan cycle.
   Phase 4 is the largest: plugin `.mcp.json` entries are stdio
   (`{"command":"npx",...}`) while `mcp-client` is Streamable-HTTP-only and runs
   in the server process, never the sandbox — a new transport *and* a new home
   for the client.

## Artefacts

- Specs: `docs/superpowers/specs/2026-08-02-plugin-marketplace-design.md`,
  `docs/superpowers/specs/2026-08-02-plugin-hooks-design.md`
- Plans: `docs/superpowers/plans/2026-08-02-plugin-manifest-resolver.md`,
  `2026-08-02-cli-marketplace-registry.md`, `2026-08-02-plugin-hooks-dispatch.md`
- The hooks spec has been rewritten for the runtime-side architecture; the hooks
  *plan* still describes the old server-side design and is stale.
