# Session tags (replacing groups) & header alignment — design

Date: 2026-08-13
Status: approved

## Intent

Two things, shipped together because both touch the rail and the pages beside
it.

**1. Groups become tags.** A session belongs to exactly one group today. That
is the wrong shape: a session is "the web UI work" *and* "the thing I'm
shipping this week", and a single-parent taxonomy makes you pick. Tags replace
groups outright — zero or more per session, created by using one, gone when
the last session drops it.

**2. Header chrome.** Four list pages drifted off the app's single header
height, and two of them off its background colour. The nameplate is not a link
home.

Requirements (from the user):

- A tag is created by assigning a name that does not exist yet. There is no
  create-tag step and no tag registry.
- A tag is deleted by removing it from every session. There is no delete-tag
  control.
- A session carries zero, one, or many tags.
- Sessions are filtered by tag. A tag can be *required* or *excluded*; every
  active constraint ANDs with the others.
- A filter button sits at the right of the `Sessions` section title. Clicking
  it reveals the tag chips at the top of the session list, below the title.
- The "new group" button is gone.
- Agents / Environments / Routines / Workflows page headers match the session
  detail header in height and background.
- Clicking the HORSIE nameplate navigates home.

Explicitly out of scope: backward compatibility. Existing `group=` annotations
and `Group*` journal events are dropped, not migrated.

## Data model — a tag is an annotation key

Sessions already carry `annotations: BTreeMap<String, String>` in
`SessionRecord`, journalled through `SessionAnnotationsSet` and edited through
`PUT /api/sessions/{id}/annotations`. Tags ride that mechanism with **no new
backend state, no new endpoint, and no migration**:

| Operation | Request |
|---|---|
| assign `web` | `set: [{key: "tag.web", value: ""}]` |
| unassign `web` | `remove: ["tag.web"]` |
| read | the `annotations` already on `SessionSummary` |

The value is unused and always empty — presence of the key *is* the tag. The
`tag.` prefix keeps tags in their own namespace, so a future `source=` or
`origin=` annotation cannot collide with a tag named `source`.

**Tag names are lowercase `[a-z0-9._-]`, 1–124 characters.** That is not a new
rule: `valid_annotation_key` in `crates/server/src/http/groups.rs` already
enforces exactly this charset over the whole key, and the 124 falls out of its
128-character limit minus `tag.`. A tag containing a dot (`v2.migration`) is
legal and unambiguous, because the prefix is stripped once, not split on.

The frontend normalises what the user types — trim, lowercase, collapse
internal whitespace to `-`, strip anything else — so `Bug Fix` becomes
`bug-fix` rather than being rejected. A name that normalises to empty is
refused with no request sent.

**The tag universe is derived, never stored.** The rail already holds every
session with its annotations, so the set of known tags is a fold over that
list. This is what makes both halves of the user's requirement free: assigning
an unknown name creates the tag (it now appears in the fold), and removing the
last assignment deletes it (it no longer appears). There is nothing to
register and nothing to garbage-collect.

## What gets deleted

The group registry existed to let an empty group survive with no sessions in
it. Tags have no such concept, so the whole subsystem goes.

**Server** (`crates/server/src/`):

- `http/groups.rs` → renamed `http/annotations.rs`, keeping only
  `set_annotations`, `valid_annotation_key`, and their tests.
- `http/mod.rs`: the `/api/session-groups` and `/api/session-groups/{name}`
  routes, and the group round-trip tests.
- `sessions/supervisor.rs`: `CreateGroup`, `RenameGroup`, `DeleteGroup`,
  `ListGroups` commands; `GroupCreated`, `GroupRenamed`, `GroupDeleted`
  events; `GroupRecord`, `GroupError`, `validate_group_name`, `group_exists`,
  `GROUP_NAME_MAX_LEN`, and `SessionSupervisorState.groups`.

**Models** (`crates/models/fluorite/session_api.fl`): `SessionGroupView`,
`CreateGroupRequest`, `CreateGroupResponse`, `RenameGroupRequest`,
`ListGroupsResponse`, plus the generated TS under
`clients/web/src/generated/session_api/`.

**Web** (`clients/web/src/`): `components/SessionGroupSection.tsx`,
`lib/sessionGroups.ts`, `hooks/useGroups.ts`, the `sessionGroups` client in
`api/client.ts`, and their tests.

### Journal consequence, stated plainly

`SessionSupervisorEvent` is a persisted enum. Removing three of its variants
means a journal that already recorded a `GroupCreated` no longer decodes.
Per the user's decision this is a clean break: **an existing deployment must
have its supervisor journal state cleared on upgrade.** The implementation
confirms the exact failure mode (hard error vs skipped event) and records it
in the PR body, so the homelab deploy is not surprised by it.

## Rail — the flat list

`Sidebar.tsx` loses sections. Rows render in the order the API returns them,
under one `nav`. Gone with the sections: the persisted group order
(`horsie.session-group-order`), the persisted collapse set
(`horsie.session-groups-collapsed`), `GROUP_DRAG_MIME`, drag-to-group,
drag-to-reorder, and `moveBefore` / `reconcileOrder` / `partitionSessions`.
Session rows keep their own `draggable` only if something still consumes it;
nothing does, so it goes too.

### The section title row

```
Sessions                                    [filter] [+]
```

The folder-plus button is replaced by a filter button (lucide `ListFilter`),
`data-testid="tag-filter-button"`. It is a toggle: `aria-expanded` reflects
whether the tag panel is open. When any constraint is active the button
carries the active-control treatment used elsewhere on the rail
(`bg-raised` + inset ring) so a filtered list is never silently filtered —
the one failure mode of a collapsible filter is looking at a short list and
believing it is the whole list.

The button renders only when at least one tag exists anywhere. With no tags
there is nothing to filter by, and an empty panel behind a button is a control
with no job.

### The tag panel

Between the title row and the list, shown only while open: every known tag,
name-sorted, as a chip. Each chip cycles through three states on click:

| State | Meaning | Rendering |
|---|---|---|
| neutral | no constraint | default chip |
| require | session must carry it | accent fill, check glyph |
| exclude | session must not carry it | muted fill, strike/minus glyph |

`aria-pressed` cannot express three states, so each chip is a plain button
whose accessible name states its state (`web — required`, `web — excluded`,
`web`), and `data-state="neutral|require|exclude"` carries it for tests.

A `Clear` action appears in the panel whenever anything is active.

Filter state persists across reloads in `horsie.session-tag-filter` as
`{require: string[], exclude: string[]}`, reconciled against the live tag
universe on read (a constraint naming a tag nobody carries any more is
dropped). Group order and collapse were persisted; a filter that silently
resets on reload would be the one piece of rail arrangement that does not
survive.

### Filtering

Two filters now narrow one list, and they compose: the existing text box
matches title/workflow, the tag constraints match annotations, and a session
must satisfy both. Empty results need to say which filter emptied them — "no
session matches X", "no session carries these tags", or both — because an
empty rail otherwise reads as an account with no sessions.

## Session row — the tag menu

`SessionRow.tsx`'s `⋯` menu replaces "move to group" with tag editing:

- Every known tag, name-sorted, as a checkable item. A tag this session
  carries shows a check; selecting toggles it. The menu stays open across
  toggles — assigning two tags should not cost two trips.
- Below them, a text input (`data-testid="new-tag-input"`). Enter normalises
  the name, assigns it, and clears the input. This is the only way a tag comes
  into existence.
- A separator, then `Delete session`, unchanged.

Rows themselves are unchanged — no tag chips on the row. The rail is already
dense with a lamp, a title, a workflow, a status and a timestamp; tags live
one click away in the menu that already edits them.

## Header alignment

`h-[3.25rem] … border-b bg-panel` is the app's header contract — the rail
nameplate, the task panel, `SessionView`, `SessionUnavailable`,
`AgentEditPage` and `EnvironmentEditPage` all honour it. Six headers do not:

| File | Today | Wrong |
|---|---|---|
| `pages/agents/AgentsPage.tsx` | `bg-panel px-4 py-3.5` + subtitle | height |
| `pages/environments/EnvironmentsPage.tsx` | `bg-panel px-4 py-3.5` + subtitle | height |
| `pages/workflows/WorkflowsPage.tsx` | `px-6 py-4` | height, bg |
| `pages/workflows/WorkflowDetailPage.tsx` | `px-6 py-4` | height, bg |
| `pages/workflows/WorkflowEditPage.tsx` | `px-6 py-4` | height, bg |
| `pages/routines/RoutinesPage.tsx` | `px-6 py-4` | height, bg |
| `pages/routines/RoutineDetailPage.tsx` | `px-6 py-4` | height, bg |
| `pages/routines/RoutineEditPage.tsx` | `px-6 py-4` | height, bg |

All become
`flex h-[3.25rem] shrink-0 items-center gap-2 border-b bg-panel px-4 sm:gap-3 sm:px-6`.
The Agents and Environments subtitle lines are deleted: at 3.25rem there is no
room for a second line, and both pages' empty states already say what the page
holds.

The user named the four list pages; the detail and edit pages carry the
identical defect and are fixed in the same pass rather than left as a visible
seam between a list and the page it links to.

## Nameplate

The `h` chip and the `HORSIE` wordmark become one `Link to="/"`
(`data-testid="home-link"`), with the rail's standard hover treatment. The
offline lamp stays outside the link — it is status, not a destination.

## Testing

**Rust.** Deletions, so mostly test removal. What remains: `annotations.rs`
keeps its key-validation tests, and `supervisor.rs` keeps
`annotations_set_and_removed_fold` and `session_delete_drops_its_annotations`.
The three group-fold tests and the HTTP group round-trip go with their
subject.

**Web unit (vitest).** `lib/sessionTags.ts` is pure and carries the load:

- `sessionTags(summary)` — extracts and sorts `tag.*` keys; ignores non-tag
  annotations; a key of exactly `tag.` is not a tag.
- `allTags(sessions)` — deduped, sorted union; a tag vanishes when its last
  carrier does.
- `normalizeTagName(raw)` — casing, whitespace, illegal characters, empty
  result, over-length.
- `matchesTagFilter(summary, filter)` — every require/exclude combination,
  including require and exclude of the same tag (matches nothing) and an empty
  filter (matches everything).
- `reconcileFilter(saved, universe)` — drops constraints for dead tags.

`Sidebar.test.tsx` covers the panel toggling, the button's active treatment,
tri-state cycling, `Clear`, and the two empty-state messages.
`SessionRow.test.tsx` covers toggling a tag on and off and creating one from
the input, asserting the exact annotation mutation each sends.

**E2e (Playwright).** `s-session-groups.spec.ts` becomes
`s-session-tags.spec.ts` over the real server: tag a session from its menu,
see the tag appear in the filter panel, require it, watch an untagged session
leave the list, exclude it, watch the tagged session leave, clear, untag, and
watch the tag disappear from the panel entirely.

**By eye.** The rail (panel open, panel closed, tag menu open) and the six
headers, both themes, screenshotted from the real stack before the PR is
called ready. A green suite cannot tell us a chip was never drawn.
