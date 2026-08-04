# Session annotations & groups — design

Date: 2026-08-04
Status: approved (pending user spec review)

## Intent

Sessions gain user-set **annotations**: key-value metadata attached to a
session. The first consumer is **groups**: a session annotated with
`group=<name>` renders under that group in the web sidebar. Future consumers
(provenance like `source=routine:<name>` for routine/agent/workflow-created
sessions) ride the same plumbing and are explicitly **out of scope** for this
iteration.

Requirements (from the user):

- Groups CRUD API.
- "Add group" button in the sidebar's Sessions header, left of the create
  session button.
- The sidebar's group list is the union of the groups API and the `group`
  annotations present in the session list.
- Rename and delete a group via a `...` dropdown on the group section header.
  Deleting a group also strips the `group` annotation from sessions under it.
- An "Ungrouped" section exists, created purely at the frontend.
- Groups (including Ungrouped) can be reordered; the order persists
  frontend-only.
- The sessions list API stays flat: a list of sessions carrying annotations,
  not sessions grouped by group.
- A session is assigned to a group both via a per-row `...` menu ("Move to
  group") and via drag-and-drop onto a group header.

## Data model — supervisor state & journal events

Everything durable about sessions lives in the `SessionSupervisor` journal;
annotations and groups follow suit. **No new DB tables, no migration.**

`SessionRecord` (server/src/sessions/supervisor.rs) gains a field:

```rust
pub struct SessionRecord {
    pub spec: SessionSpec,
    pub created_at: u64,
    /// User-set key-value metadata (group, future provenance keys).
    /// Field-level default so pre-annotations snapshots load with an empty map.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}
```

This honors the state's snapshot durability contract ("add optional fields;
never rename or repurpose one") the same way `SessionSpec` already does for
`origin` and `plugins`. Putting annotations on the record — rather than in a
separate map keyed by session id — means `SessionDeleted` automatically drops
a session's annotations, and the `List` command already returns everything
`SessionSummary` needs.

`SessionSupervisorState` gains the group registry:

```rust
pub struct SessionSupervisorState {
    pub sessions: BTreeMap<SessionId, SessionRecord>,
    /// Registered groups, name-keyed. A group may have zero sessions, and an
    /// annotation may reference an unregistered group.
    #[serde(default)]
    pub groups: BTreeMap<String, GroupRecord>,
}

pub struct GroupRecord {
    pub created_at: u64,
}
```

New journaled events on `SessionSupervisorEvent`:

- `GroupCreated { name, created_at }`
- `GroupRenamed { old, new }` — the fold renames the registry key **and**
  rewrites `group=<old>` → `group=<new>` across every session's annotations.
- `GroupDeleted { name }` — the fold removes the registry key **and** strips
  `group=<name>` annotations from every session.
- `SessionAnnotationsSet { id, set: BTreeMap<String, String>, remove: Vec<String> }`
  — merge semantics: each `set` entry upserts a key, each `remove` entry drops
  one. One journal append per API call, so a multi-key update is atomic.
  Generic: any annotation keys ride this event, present and future.

Rename/delete fixups happen in the event fold, so they are atomic with the
journal append — no fan-out, no partial state.

New supervisor commands mirror the events:

- `CreateGroup { name, created_at, reply }` — error on duplicate.
- `RenameGroup { old, new, reply }` — error if `new` already exists; error if
  `old` is neither registered nor referenced by any annotation (see semantics
  below).
- `DeleteGroup { name, reply }` — same existence rule.
- `ListGroups { reply }` — the registry, name-sorted.
- `SetSessionAnnotations { id, set, remove, reply }` — error if the session is
  unknown.

### Rename/delete semantics for unregistered groups

The sidebar's group list is a union, so a group can exist only because
sessions reference it. Rename and delete therefore operate on **annotations
regardless of registry membership**: renaming an annotation-only group
rewrites the annotation values (and does not register the new name); deleting
one strips them. A name that is neither registered nor referenced gets a 404.

## HTTP API & wire types (fluorite)

New routes in server/src/http/mod.rs:

| Route | Handler | Notes |
|---|---|---|
| `GET /api/session-groups` | list groups | `ListGroupsResponse { groups }` |
| `POST /api/session-groups` | create | `CreateGroupRequest { name }`; 409 on duplicate |
| `PUT /api/session-groups/:name` | rename | `RenameGroupRequest { name }`; 404 / 409 |
| `DELETE /api/session-groups/:name` | delete | strips annotations; 404 if unknown |
| `PUT /api/sessions/:id/annotations` | merge keys | `SetAnnotationsRequest { set, remove }` — upsert `set`, drop `remove`; other keys untouched |

Fluorite schemas (models/fluorite/):

- `session.fl`: `SessionSummary` and `SessionDetail` gain
  `annotations: Vec<AnnotationEntry>` where
  `struct AnnotationEntry { key: String, value: String }`. (Fluorite has no
  map type in use; a vec of entries is the established shape.)
- `session_api.fl`: `AnnotationEntry` (shared), `SessionGroupView { name }`,
  `CreateGroupRequest`, `RenameGroupRequest`, `ListGroupsResponse`,
  `SetAnnotationsRequest { set: Vec<AnnotationEntry>, remove: Vec<String> }`.

"Move to group" from the UI is `PUT annotations` with `set=[group=<name>]`;
"move to Ungrouped" is `remove=["group"]`. Merge semantics mean the UI never
clobbers annotation keys it doesn't know about.

Validation: group names and annotation keys are non-empty and length-capped
(128 chars); keys match `[a-z0-9._-]+`. Group names are free-form display
strings (not slugs) since they are user-facing labels.

## Frontend — sidebar (clients/web/src/components/Sidebar.tsx and new components)

- **Group sections**: collapsible. Sessions partition by their `group`
  annotation. The section list is the union of the groups API and the `group`
  values seen in the session list. **Ungrouped** is a frontend-only sentinel
  section, always rendered, never sent to the API.
- **Sessions header**: a group-add button (`FolderPlus` icon, matching the
  existing `key-icon` style) at the left of the current `+` button. Opens an
  inline name input; submit → `POST /api/session-groups`.
- **Group header `...` dropdown**: Rename (inline edit) / Delete (confirm
  dialog noting annotations are stripped). Requires a small dropdown-menu
  component — none exists in the UI today; built to match the skin
  (`key-icon`, `--radius-control`, panel/legend tokens).
- **Session row `...` menu**: "Move to group" submenu listing all groups plus
  "Ungrouped".
- **Drag-and-drop**: HTML5 DnD, no dependency. Dragging a session row onto a
  group header assigns the group (same mutation as the menu). Dragging a group
  header reorders sections.
- **Ordering**: an ordered list of group names, with `"ungrouped"` as the
  sentinel entry, persisted via `usePersistentState` in localStorage
  (frontend-only, per requirements). New groups append; entries for vanished
  groups are dropped on reconcile.
- **Data**: React Query — a `useGroups` hook plus mutations that invalidate
  the sessions and groups queries. No SSE changes: the global event stream is
  untouched.
- **Collapse state**: per-section collapsed flag, ephemeral component state
  (not persisted).

## Edge cases

- Deleting a session drops its annotations (they live on the record).
- A session is in at most one group (single `group` key, last write wins).
- Routine-run sessions remain filtered out of the session list; their
  annotations are unaffected and unreachable from the sidebar.
- Session ordering within a group is unchanged (current list order).
- Renaming to an existing name → 409, nothing changes.
- Group names are case-sensitive.

## Testing

- Rust unit tests alongside `supervisor.rs`: event folds (rename carries
  annotations across sessions, delete strips them, session delete drops
  annotations, annotation set/remove), command validation (duplicate create,
  rename onto existing, unknown group 404 rule).
- HTTP tests in the existing `http/mod.rs` style: group CRUD round-trips,
  annotation set, list-sessions merging annotations into `SessionSummary`.
- Web: unit tests for the group-union/partition logic and order-persistence
  reconcile; component tests for the dropdown menu. Extend the existing
  sidebar e2e spec if one exists.

## Out of scope

- Auto-writing provenance annotations (`source=…`) for routine/agent-created
  sessions.
- Assigning a group at session-creation time.
- Persisting group order or collapse state server-side.
- Multi-group membership.
