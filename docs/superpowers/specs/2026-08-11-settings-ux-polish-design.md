# Settings and session-list UX polish

Eight small defects in the web UI, each one a place where the interface
contradicts itself.

## The problem

None of these is a bug in the sense of producing a wrong answer. Each is a place
where a person doing an ordinary thing meets a surface that behaves differently
from the one beside it, and has to work out why.

- **Deleting a provider is refused rather than done.** Removing a provider is
  the only way to be rid of it, and the models routed through it are only useful
  *because* of it, but the delete stops and asks you to go and delete each model
  by hand first. There is no case where someone wants the models and not the
  provider.
- **Two destructive actions have no confirm and the rest do.** MCP servers and
  machine tokens delete on the first click. Every other delete in the app raises
  `askConfirm`, so a person who has learned "a delete asks first" is wrong
  exactly twice, and both times irreversibly.
- **Auto-compaction is a switch nobody should be asked about.** It sits in the
  new-session config row beside the model and the skills, giving a decision
  equal weight to those, when the only sensible answer is "yes, keep the session
  alive".
- **Instructions is the widest field in the agent form and the narrowest
  control.** It is a textarea sharing a two-column grid with two single-line
  inputs, so it renders at the width of the Name box.
- **The runtimes page lists cloud vendors twice.** `settings.vendors` is a live
  roster that a saved cloud vendor joins the moment it exists, so it appears
  under *Connected vendors* and again under *Cloud vendors*. Editing one opens
  its form at the very foot of the page — nowhere near the row clicked — and the
  delete button lives inside that form rather than on the row, with no confirm.
- **A session can only be deleted from inside itself.** The rail's row menu
  offers moves between groups and nothing else, and it does not appear at all
  until a group exists.
- **A long provider name runs out of the panel.** `Models · <name>` and *No
  models route through `<name>` yet* are both unconstrained text in a fixed
  panel.

## Approach

### Provider delete takes its models with it

`DbConfigStore::delete_provider` currently reads the models, finds the ones
referencing the provider, and returns an error naming them. That check becomes a
delete: inside the same write transaction, `DELETE FROM models WHERE user_id = ?
AND provider = ?` runs before the provider row goes.

The safety this replaces is not lost. `validate_and_commit` rebuilds the
registry from the transaction's pending state before committing, and that
rebuild is what rejects a model routed to a provider that does not exist — so a
cascade that missed a row still cannot commit.

The refusal is gone from the API rather than hidden behind a flag. Two
behaviours on one route means every caller has to know which one it gets, and
there is no caller that wants the refusal.

The web confirm carries the consequence instead of the server carrying the
guard:

> Delete provider "openrouter" and its 3 models (sonnet, haiku, kimi)?

with the plain `Delete provider "x"?` when it has none. `ModelsSettings`'
client-side orphan check is deleted along with the server's.

### The two missing confirms

`askConfirm` already exists for exactly this. `McpServerRow.remove` and the
machine-token Revoke button each get one, worded with the name of the thing and,
for a token, what stops working:

> Revoke machine token "ci"? Anything using it stops connecting.

The GitHub section's *Disable* button also deletes an MCP server — the `github`
one — and deliberately does not get a confirm. It reads as a toggle, re-enabling
is one click, and it destroys no configuration a person typed.

### Auto-compaction stops being a question

The picker goes from `configPickers`, and `autoCompact` from `useSessionDraft`,
`draftPersistence` and `useAgentDraft`. The request field is then simply
omitted, which the server already reads as on.

The wire types keep `auto_compact`. It stays a real setting an API caller can
turn off; it is only the UI that stops asking, and re-adding the control later
costs nothing.

### The runtimes page becomes one list

The duplication is not a rendering mistake — it is two lists over sources that
overlap. `settings.vendors` is the live roster (dialled-in agents *and* saved
cloud vendors), `useRuntimeVendors()` is the configuration of the cloud ones.
Neither is a subset the other can be derived from, so the fix is one list joined
on name:

- **In the roster and in the cloud config** — a cloud vendor. Subtitle
  `Fly · iad · <image>`, chips for its token and its last check, actions Check /
  Edit / Delete plus Make default.
- **In the roster only** — an agent process that dialled in. It carries its own
  configuration where it runs, so there is nothing here to edit: Make default,
  and that is all.
- **In neither, but named as the default** — the existing "set as default but
  not connected" row, unchanged. A preference for a machine that is currently
  off is legitimate and has to stay visible.

Edit expands the form *inside* the row. `ListRow` already renders `children`
below its identity line — the same affordance the model and provider lists use —
so this is the page adopting a pattern it already contains rather than a new
one.

Delete moves onto the row, marked danger, behind a confirm. A destructive action
reachable only by first opening an edit form is a strange place to keep one, and
it was the only delete in the app with no confirm and no row.

Adding stays at the foot of the section, as a form below the list. A vendor that
does not exist yet has no row to expand into.

`CloudVendors.tsx` reduces to `VendorForm` plus its helpers (`withConnectPath`,
`summarise`, the empty settings). `RuntimesSettings.tsx` owns the single list and
the draft state. The split is by what the pieces are, not by which section they
used to be under: one component renders a vendor's fields, the other decides
which vendors exist.

### Delete session on the rail

`SessionRow`'s menu stops being gated on `groups.length > 0` and always renders.
It carries the move-to-group items when there are groups, then a danger **Delete
session** behind the same confirm `SessionView` raises. Deleting the session
currently open navigates to `/`, because the alternative is a view of something
that no longer exists.

### Long names wrap

`Section`'s heading and its empty-state paragraph get `break-words`. Wrapping
rather than truncating: the provider name is the subject of both strings, and a
heading reading `Models · openrouter-eu-…` has lost the thing it was naming.

## What is not changing

- Model and provider deletes keep their current confirms and wording.
- `auto_compact` stays in the wire types and in the server's behaviour.
- The runtime vendor API is untouched; the merge is entirely a client-side join.
- Nothing about how sessions are deleted — the row menu calls the same hook the
  session view does.

## Verification

- A store unit test replacing
  `delete_provider_is_blocked_while_a_model_references_it`: a provider with two
  models deletes, and both models are gone afterwards.
- Component tests for each new confirm, for the merged runtime list (one row per
  vendor, cloud actions only on cloud rows), and for the session row's delete
  item.
- `npx tsc -b` — `tsc --noEmit` is a no-op against this solution-style config.
- The web e2e specs that name the runtime testids, updated for the merged rows.
