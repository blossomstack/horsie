# Workflow UI: editor shell, horizontal graph, and running from the new-session page

Date: 2026-08-04

The workflow UI shipped in #188 as three pages that each work but do not scale:
the editor is one scrolling column of accordions, the graph flows top-to-bottom
in a 26rem gutter, and starting a run offers a text box and nothing else — no
runtime, no repos — even though the API has accepted both since #184.

This redesign gives the editor a shell, turns the graph on its side, and folds
starting a run into the new-session page instead of keeping a second launch
surface.

## 1. Editor shell

`WorkflowEditPage` becomes master-detail.

**Sidebar.** A `Definition` row first (name, description, start step), then one
row per step, then `Add step`, then a `Visualize` toggle. Selecting a row puts
its editor in the right panel; exactly one row is selected at all times.

Step rows reorder by dragging a handle, and the handle is a focusable button
that also responds to `ArrowUp`/`ArrowDown` — native HTML5 drag is not keyboard
operable, and this list is the only place order can be changed. Reordering
permutes the `steps` array and nothing else: execution follows `start` and the
transitions, so order is reading order. Delete stays a per-row button.

**Right panel.** The selected definition or step, as a plain form with no
accordion — agent, prompt, output fields, transitions. When `Visualize` is on,
the panel shows the graph instead, and clicking a node selects that step and
returns to its form. That link is the reason the graph moves into the panel
rather than staying beside it: the current preview cannot navigate.

Header, `Save`, the request body, and the `StepDraft` / `rawSchema` handling are
untouched.

## 2. Horizontal graph

`WorkflowGraph` flows left to right: rank on x, order-within-rank on y. Edges
leave a node's right edge and enter the next node's left edge as cubic béziers
with horizontal control points, with the condition label above the midpoint —
which is where a horizontal edge has room for one and a vertical edge does not.
Back-edges bow below the row so a loop still reads as a return.

`layoutGraph` is already orientation-neutral: it emits `rank` and `order`, and
only the positioning and path math in the component change. Its `width` and
`height` return fields are renamed `breadth` and `depth`, because "width = how
many nodes stack vertically" is an actively wrong name once the graph is
horizontal.

Orientation is fixed, not a toggle. Both the editor preview and
`WorkflowRunView` render through this component, so both turn together.

## 3. Running a workflow from the new-session page

`POST /workflows/:name/runs` already takes `vendor`, `repos`, and `name` beside
`input`. Rather than build a second launch surface to expose them, runs join the
one that exists.

`SessionConfigBar` in draft mode gains a **Workflow** key, leftmost, listing
`None` plus every definition.

- **None** — the page behaves exactly as today.
- **A workflow** — only `Runtime` and `Repos` remain. `Model`, `Thinking`,
  `Skills`, `MCP`, and `Memory` are hidden, because a run takes each of those
  from the step's own agent preset and `WorkflowRunRequest` has no field to
  override them. Rendering controls whose values are never sent would be a lie
  about what the button does.

The composer's text is the run input. Sending calls `useRunWorkflow` rather than
`useCreateSession`, then navigates to the run's session, where `WorkflowRunView`
already takes over. There is no pending-first-message hand-off: the input is
part of the create call, not a message sent after it.

`blockedReason` in workflow mode drops the model requirement, keeps runtime, and
keeps "connect GitHub" when the chosen vendor provisions.

The selection is **not** persisted. `DraftPayload` is untouched and the picker
starts at `None` every visit — a stored workflow would mean opening "New
session" days later and silently being in a mode you did not choose, which the
other channels do not do because they only tune a session rather than replace
what the page starts.

## 4. Run button on the workflow page

`WorkflowDetailPage`'s inline "Run it" panel is deleted. The header gets `Run`
beside `Edit`, navigating to `/?workflow=<name>`. `NewSessionView` reads that
search param once on mount to preselect the picker; a URL is used rather than
router state so the link survives a reload and can be shared. The body becomes
description, graph, and run history.

## 5. Testing

Workflows have no test coverage at all today — #188 shipped without a unit test
or an e2e spec. This adds:

- `graphLayout.test.ts` — updated for `breadth`/`depth`.
- Unit tests for the editor's selection/reorder reducer, kept pure and outside
  the component for exactly that reason.
- Unit tests for the workflow-mode picker set and `blockedReason`.
- `e2e/t-workflows.spec.ts` — define a workflow, reorder its steps, visualize,
  then start a run from the new-session page with a runtime selected.

## Out of scope

- Any change to the workflow API, the orchestrator, or the run projection.
- The plugin-union limitation tracked in #182.
- A graph orientation toggle, and step reordering that means anything to
  execution.
