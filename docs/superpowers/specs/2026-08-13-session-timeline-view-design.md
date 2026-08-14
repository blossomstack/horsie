# Session timeline view

A second way to read a session: the shape of the work, drawn as a horizontal
timeline, instead of a scroll you have to walk.

## The problem

A session's transcript is a single vertical column, and every structural fact
about the session is buried somewhere inside it. How many subagents ran, when
they ran, how long any of them took, where the conversation forked, how much of
the elapsed wall-clock was the machine working versus the session sitting idle
overnight — all of it is in there, and none of it is visible without scrolling
past everything else.

The pieces that carry that structure today are all inline and all local:
`SubAgentCard` shows a subagent where its result landed, `ForkMarker` shows a
branch where it was taken, `CompactionDivider` shows a boundary where it fell.
Each is correct in place and says nothing about the whole. `TranscriptSpine`
is the only overview that exists, and it is deliberately just compaction
boundaries down one edge.

What is missing is a view where the session's *structure* is the subject and
the prose is the detail — the inverse of the transcript.

## Approach

One horizontal timeline per agent, stacked into lanes. The main agent is the
top lane and carries one bar per transcript entry, coloured by kind. Every
subagent and every fork gets its own lane below, drawn as a single span with an
arrow pointing back to the moment on its parent's lane that it came from.
Everything is clickable.

Five decisions were settled before this was written, and they are what keeps it
small:

| | chosen | rejected |
|---|---|---|
| horizontal axis | compressed time | real wall-clock; pure sequence |
| bar granularity | one per transcript entry | one per turn |
| how much loads | main agent's bars only | every agent's history |
| overflow | horizontal scroll at fixed scale | fit-to-width; zoom control |
| clicking a bar | jumps into the transcript | side detail panel |

"Main agent's bars only" is the load-bearing one. It means the view needs
exactly the two reads the session page already makes — `GET /sessions/:id` and
the main agent's message stream — and adds no fan-out over the roster. A
subagent lane is a span, not a strip of bars, and clicking it navigates to that
agent's own page, which already exists.

## What the data already gives us

Most of it, and the gap is smaller than it first looks.

`SessionDetail.agents` is a flat `Vec<SubAgentView>` carrying the main agent
and its whole subagent tree, each entry with `parent`, `depth`, `label`,
`status`, `spawned_at_ms` and `ended_at_ms`. That is every subagent lane and
its exact span, in a request the page already makes.

Three things that data does *not* say, and what each one costs:

**It does not carry forks.** `agent_roster` builds `main_entry` (or one
`step_entry` per workflow execution) plus the subagent tree, and stops there —
`fork_entry` exists, but only `read_agent` uses it, for a single agent's own
document. A session's forks live on `SessionRecord.forks` in the supervisor,
kept current by `ForksChanged`, and reach the wire only through
`SessionSummary.forks` for the session list. This is the one server change the
feature needs; see below.

**A fork has no end, and never had one.** `ForkView` is `id`, `parent`,
`title`, `status`, `created_at_ms` — there is no end stamp to want, and
`fork_entry` says why: "a conversation is never *done*". So a fork lane is
drawn as a start marker and an open bar running to the right edge, never as a
measured span. A subagent lane, whose `spawned_at_ms`/`ended_at_ms` are both
real, is.

**A tool call has no start time.** `ToolResultPart` carries `tool_call_id`,
`output` and `is_error` and no timestamps at all. A tool's duration is
therefore the interval from the assistant message that issued it
(`created_at_ms`) to the result that answered it — which is exactly what
`transcriptSegments.ts` already computes for its work-group spans, so this
introduces no new notion of time. Parallel tool calls in one assistant message
correctly share a start and differ at the end.

Timing for the other kinds comes off `Message` directly: an assistant message
has both `started_at_ms` (provider call issued) and `created_at_ms` (stream
finished), so thinking and text have a true duration. A user message has only
`created_at_ms` and is a point event.

## What it is made of

### The toggle

A `key-icon` button in the session header, immediately after `SessionTitle` and
before the status badge, styled like the existing `task-list-toggle`: a ring
when the timeline is showing. It sits with the title rather than in the
right-hand key cluster because it changes *what you are looking at*, and that
cluster is for acting on what you are already looking at.

State lives in the URL as `?view=timeline`, not in component state or
`usePersistentState`. A view of a session is a thing you send someone.

The timeline replaces the transcript pane at full width; the header, config bar
and composer stay exactly where they are, so a session can be driven while the
map is up. A workflow run is unaffected — it already renders `WorkflowRunView`
instead of a transcript and returns before this code is reached.

### The lane model — `lib/timeline.ts`

One pure function, `buildTimeline(items, agents, forks, nowMs)`, taking the
main agent's `TranscriptItem[]`, the session's subagent roster and its fork
list, and returning a laid-out model in pixels. Pure and separate for the same reason `transcriptSegments.ts`
and `forkTree.ts` are: the placement rules are where the bugs live, and they
should be testable without a DOM.

```ts
export type LaneKind = "main" | "subagent" | "fork";

export interface Bar {
  key: string;
  kind: "user" | "assistant" | "thinking" | "tool" | "ask" | "compaction";
  x: number;
  width: number;
  /** What a click scrolls the transcript to. A compaction tick carries the
   * boundary's seq, which `TranscriptSpine` can already seek to. */
  entryId: string;
  /** Tooltip: what it is, and how long it took. */
  title: string;
  detail: string;
  /** Still running — drawn with the live edge. */
  live?: boolean;
}

export interface Lane {
  agentId: string;
  kind: LaneKind;
  label: string;
  status: string;
  depth: number;
  /** Populated only for the main lane. */
  bars: Bar[];
  /** Absent for the main lane, and for any agent with no usable stamps. */
  span?: { x: number; width: number; open: boolean };
  /** Where on the parent lane this one hangs from. */
  anchor?: { x: number; parentAgentId: string };
}

export interface Timeline {
  lanes: Lane[];
  /** Collapsed idle stretches, drawn as a gutter with what they swallowed. */
  gaps: { x: number; elapsedMs: number }[];
  /** One per turn start, labelled with wall-clock time. */
  ticks: { x: number; label: string }[];
  width: number;
}
```

### Compressed time

The horizontal axis is wall-clock order with the dead air taken out.

The layout walks the main agent's entries in start order, carrying a pixel
cursor. Between two entries it advances the cursor by the real elapsed time
times a scale factor — unless that gap exceeds 60 seconds, in which case it
advances a fixed 24px and records a gap marker labelled with what was skipped
(`⋯ 3h 12m`). Either way a gap is capped at that same 24px, so waiting never
outdraws working. A bar's width is its duration times the scale, with a 6px
minimum so a fast tool call stays hittable. Nothing caps the top.

**The scale is set by the longest single span**, which is drawn at 240px;
everything else is in proportion to it. This is the second version of the rule.
The first scaled the session's *total* active time to a fixed 2400px and then
clamped the scale to between 0.5 and 20 pixels per second, reasoning that a
short session should not be stretched absurdly. That reasoning was wrong, and
wrong in a way no unit test could catch: at 20px/s a session that finished in
three seconds drew as a 100px smudge in a 1000px pane. Scaling off the longest
bar makes a three-second session and a three-day one equally readable, and it
removes the need for a maximum bar width at all — the longest bar *is* the
scale, so no bar can run away.

A subagent result is not a bar. It lands in the parent's transcript inside a
user message, and `transcriptSegments.ts` renders it as a `SubAgentCard` there
— but here it already has a lane of its own, and drawing it twice would say the
work happened twice.

Because widths are clamped and gaps are collapsed, the drawn axis is a
monotone but non-linear function of time. That has one consequence worth
stating: **evenly spaced time ticks are impossible**. Instead a tick is emitted
at each turn start, labelled with the wall-clock time that turn began. This is
more useful than a regular grid anyway — "this turn started at 09:38" is a
thing you want to know; "this pixel is 09:40" is not.

The same walk produces `toX(ms)`, a sorted breakpoint list with linear
interpolation between the points, which is how every off-lane moment — a
subagent spawn, a fork's branch point — is placed. Moments before the first
entry or after the last clamp to the ends.

### Placing the other lanes

A subagent lane spans `toX(spawnedAtMs)` to `toX(endedAtMs)`, with its anchor
at the spawn. A still-running subagent has `endedAtMs === 0` and gets an open
span to the right edge. A fork lane starts at `toX(createdAtMs)` and is always
open, since a fork has no end.

Fork nesting is `forkTree()`, called as-is — it already turns a flat
parent-linked `ForkView[]` into render order with a depth per row, and it is
already exercised by the rail. Subagent nesting is its own small walk, because
`SubAgentView` carries a server-computed `depth` that `ForkView` does not.

Both walks follow the two rules `forkTree.ts` learned the hard way, because
both read journal-derived data: an agent whose parent resolves to nothing
renders at the top level rather than disappearing, and anything the descent
cannot reach is appended flat rather than silently dropped. Deleting a fork
does not delete its children.

Subagent lanes and fork lanes are separated by a divider, because a subagent is
work inside a turn and a fork is a different conversation — the same
distinction `SubAgentCard` and `ForkMarker` already draw, carried into this
view.

An agent whose stamps are all zero cannot be placed on the axis at all —
`main_entry` sets both to zero, and older journals predate the fields. Those
lanes render below everything else, unplaced and greyed, with their label and
status. Showing a conversation in the wrong place is worse than showing it
outside the timeline and saying so.

### Rendering

Plain DOM, not SVG. Bars are absolutely positioned divs inside one horizontal
scroller with a sticky label gutter down the left. `WorkflowGraph` uses SVG
because it has to route edges around ranks; here there is nothing to route.
Divs get hover, focus, keyboard activation and text selection for free, and a
few hundred of them are not a rendering problem.

A lane's connector is a 1px line under a CSS arrowhead, drawn upward out of its
own row rather than as cross-row geometry between two lanes. The parent is the
lane above — or a lane above, for a nested one — so leaving the top edge reads
as "this hangs off the timeline" without every row's height having to be known
before layout, which the fork divider and the unplaced section would otherwise
break.

All lanes share the one scroller so alignment holds while scrolling. The
scroller carries the panel surface, not just the sticky gutter: painting only
the gutter leaves each label sitting in a rectangle of a different colour from
the pane, which reads as a stray box down the left edge.

Colours come from the existing lamp and skin variables, not new ones, so
Paper/Soft/Slate all work without a fourth set of definitions.

### Clicking through

A bar on the main lane switches back to the transcript, scrolled to that entry.
The mechanism already exists — `TranscriptSpine` seeks to a compaction boundary
by querying `[data-testid="compaction-divider"][data-seq="…"]` and calling
`scrollIntoView`. The only missing piece is that ordinary messages have a
`data-testid="message"` and no identity, so `Transcript.tsx` gains a
`data-entry-id` attribute and `seek()` gains a case for it. A compaction tick
needs nothing new: it seeks by seq exactly as the spine already does.

One case the transcript cannot satisfy: an entry old enough to have been paged
out. `seek("start")` has the same limitation today and handles it by scrolling
as far back as has loaded and letting the scroll-back handler fetch the rest —
this follows that, rather than inventing a second history-loading path.

A subagent or fork lane navigates to `/sessions/:id/agents/:agentId`, which is
already a working route.

### The one server change

`SessionDetail` gains a `forks` field in `crates/models/fluorite/session.fl`,
the same shape `SessionSummary` already carries:

```
/// The conversations forked out of this session. Separate from `agents`
/// rather than mixed into it, because a fork is not a delegated task: it
/// owes nobody a result and it never ends. The server keeps them apart for
/// the same reason — `ForkRoster` is deliberately not a `SubAgentTree`.
forks: Vec<ForkView>,
```

Populated in `get_session` from `rec.forks` through the existing `wire_fork`
helper — the identical line `summary()` already uses. The supervisor keeps
`SessionRecord.forks` current whether or not the session actor is loaded, so
this costs no extra read.

A second field rather than a `kind` discriminator on `SubAgentView` because it
is both smaller and truer. Mixing forks into `agents` would need a tag to tell
them apart again at the other end, and the two kinds do not even share a
shape — a subagent has two real timestamps, a fork has one and no end.

It also means the client needs no new nesting code for forks: `forkTree()`
already places a `ForkView[]` into a lineage with depth, and the rail already
uses it.

A `.fl` edit means regenerating the type trees with `make types` in the same
commit; a codegen bump that races a PR reddens main.

## Files

| file | change |
|---|---|
| `crates/models/fluorite/session.fl` | `forks` on `SessionDetail` |
| `crates/server/src/http/handlers.rs` | populate it in `get_session` |
| generated type trees | `make types` |
| `clients/web/src/lib/timeline.ts` | new — the layout model |
| `clients/web/src/lib/timeline.test.ts` | new |
| `clients/web/src/components/SessionTimeline.tsx` | new — the view |
| `clients/web/src/pages/SessionView.tsx` | toggle, `?view=` routing |
| `clients/web/src/components/Transcript.tsx` | `data-entry-id` anchors |
| `clients/web/e2e/` | one spec |

## Testing

`timeline.ts` is pure and carries the unit tests: gap collapsing at the
threshold and either side of it, min and max width clamping, `toX`
interpolation and clamping past both ends, a subagent placed inside a tool
bar's span, a fork branching from another fork, an orphaned parent rendering
top-level, a cyclic roster appending flat rather than hanging, and an
all-zero-stamp agent landing in the unplaced group.

On the server, one test that `GET /sessions/:id` returns a fork the session
holds. The supervisor already has `forks_are_listed_without_loading_the_session`
for the list side; this is the detail side of the same fact.

One Playwright spec covers the wiring: open a session with a subagent, toggle
the view, assert the lanes, click the subagent lane and land on its page. Lane
and bar testids carry the agent id, because the suite has a history of strict-
mode failures from duplicate testids.

## Out of scope

No zoom control. No per-entry bars for subagents or forks — their lanes are
spans, and their detail is one click away on their own page. No new live
channel: the timeline redraws from the main agent's existing stream and from
the session document, and knows nothing the session page does not already know.

## Where the picture lies, deliberately

One place, marked in the UI rather than hidden: a gap is never drawn wider than
24px, whether it swallowed fifty seconds or two days, with the elapsed time
written into the gutter and the collapsed ones hatched.

Everything else — order, every bar's duration relative to every other, which
turn spawned which agent, where a fork branched — is drawn from the real
stamps.

## What only a screenshot caught

Every defect in this list passed the whole unit suite, the e2e suite, and CI.

The scale rule crushed any short session to a smudge (above). Four turns a
second apart printed four time labels at the same pixel, which read as one line
of garbled digits — ticks now drop when they would land within 56px of the
previous one. The thinking bar was outlined rather than filled, so the largest
object on the lane looked like a frame around nothing. The sticky label gutter
was `bg-panel` against a differently-coloured pane and read as a stray grey box
down the left. An open span was bounded by the drawn session's width, so a fork
taken near the end was a 13px stub that looked measured rather than open-ended.

And the connectors were never drawn at all: `Lane.anchor` was computed, typed,
and carried all the way to the component, which ignored it. "An arrow pointing
back to the moment on its parent's lane" was in the first sketch and in this
spec, and nothing in the test suite noticed it was missing.
