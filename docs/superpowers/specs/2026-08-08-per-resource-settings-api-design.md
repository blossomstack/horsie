# Per-Resource Settings API Design

## Overview

Replace the whole-document `PUT /api/config` with per-resource CRUD for providers and
models.

Today one request carries the entire `providers` list and the entire `models` list, and
the store services it by deleting every row for the user and re-inserting the list.
Two clients that each read, edit one entry, and write back will silently discard each
other's edit — last writer wins, no conflict, no error.

The storage is already row-shaped: `providers` is keyed `(user_id, name)` and `models`
is keyed `(user_id, alias)`. The whole-document shape is an artifact of the HTTP layer
alone, so **no migration is required**.

The motivating consumer is a Terraform provider, where `horsie_model` and
`horsie_provider` must be independent resources — Terraform applies them concurrently
and expects each to touch only its own row. But the race is not hypothetical for the
web UI either: two browser tabs on the Settings page reproduce it exactly.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Migration | None | `providers` and `models` are already per-row tables keyed by name/alias |
| `PUT /api/config` | Deleted outright | Keeping it would preserve the clobbering path we are removing; no backward compatibility is owed |
| `GET /api/config` | Retained | It aggregates vendors, `default_vendor` and `info`, which have no per-resource home and which the Settings page reads in one shot |
| Everything under `/api/config/` | `models` and `model-providers` | "Vendor" already means the *runtime* sandbox in horsie (`RuntimeVendor`, `/api/vendor/connect`, `default_vendor`), so a bare `/api/providers` beside it reads as if it configured runtimes |
| Providers are a sibling, not a child | `/api/config/model-providers`, not `/api/config/models/providers` | Nesting collides: a static `providers` segment under a dynamic `/{alias}` shadows any model alias literally named `providers`. Two sibling static segments cannot collide at all |
| Upsert verb | `PUT /api/config/model-providers/{name}` | The name is the identity and comes from the caller, so `PUT` to a caller-chosen URL is the honest verb; there is no server-assigned id to `POST` for |
| `default_vendor` | `PUT /api/config/default-vendor` | Genuinely a runtime concept, so it stays out of the model section. The last scalar left in `SettingsUpdate` |
| Deleting a provider models reference | `409 Conflict`, naming the models | Silent cascade would delete a session's model out from under it; the alternative — a dangling `models.provider` — is what `update` already rejects |
| Secret semantics | Unchanged: omitted `api_key` keeps the stored key, `""` clears it | Already the contract in `ProviderInput`, and the only way a client that never sees the key can round-trip a provider |
| Live registry swap | After every mutation, as today | The registry is what sessions resolve models through; a persisted-but-unapplied change is the bug this avoids |

## API

Replacing one mutating route with seven. Every route is user-scoped through the existing
`Scope` extractor, so none of them can read or write another account's rows.

| Method | Path | Body → Response |
|---|---|---|
| `GET` | `/api/config/models` | → `Vec<ModelView>` |
| `PUT` | `/api/config/models/{alias}` | `ModelInput` → `ModelView` |
| `DELETE` | `/api/config/models/{alias}` | → `204` |
| `GET` | `/api/config/model-providers` | → `Vec<ProviderView>` |
| `PUT` | `/api/config/model-providers/{name}` | `ProviderInput` → `ProviderView` |
| `DELETE` | `/api/config/model-providers/{name}` | → `204` |
| `PUT` | `/api/config/default-vendor` | `{ "vendor": "..." }` → `SettingsView` |
| `GET` | `/api/config` | → `SettingsView` *(unchanged)* |

Plural throughout, matching `/api/model-cards`, `/api/agents` and `/api/routines`.

The same vocabulary fix applies to the three existing ChatGPT sign-in routes, which are
bare `provider` today and about model providers specifically. They move with the rest:

| Before | After |
|---|---|
| `GET /api/admin/providers/{name}/chatgpt` | `GET /api/config/model-providers/{name}/chatgpt` |
| `POST\|DELETE /api/admin/providers/{name}/chatgpt/login` | `…/model-providers/{name}/chatgpt/login` |
| `POST /api/admin/providers/{name}/chatgpt/poll` | `…/model-providers/{name}/chatgpt/poll` |

Leaving them under `/api/admin/providers/` would reintroduce the exact ambiguity this
change removes.

`SettingsUpdate` is deleted. `ProviderInput`, `ModelInput`, `ProviderView` and
`ModelView` are unchanged, so the fluorite schema churn is limited to removing one
struct and adding a one-field `DefaultVendorInput`.

The `{name}` in the path is the identity; a body whose `name`/`alias` disagrees with the
path is a `422` rather than a rename, because a rename that silently moved a row would
strand the models pointing at the old name.

## Validation, Preserved Per Resource

Every rule `update` enforces today has a per-resource equivalent. These are the ones
with teeth:

- **Provider kind** must be `anthropic`, `openai`, `openai-responses` or `chatgpt`.
- **A model's `provider` must exist.** Today this is checked against the incoming list;
  per-resource it is checked against the table, which is strictly stronger.
- **A provider that models reference cannot be deleted** — `409`, listing the aliases.
  This replaces an invariant that whole-document rewrite enforced implicitly.
- **Empty name/alias** is rejected.
- **`chatgpt` kind stores no `api_key`**, and switching a provider to or from `chatgpt`
  clears its `provider_oauth` row. Wholesale rewrite got this for free by deleting
  everything; per-resource upsert must do it explicitly on the one row, and delete must
  do it too. This is the single most bug-prone part of the change.
- **Duplicate detection disappears**, correctly: one request now carries one resource,
  so a duplicate name is just an upsert.

## Transactions

Every mutation uses `Db::begin_write()`, never `pool().begin()`. Each one reads before
it writes — an upsert reads the stored `api_key` to honour "omitted means unchanged", a
delete reads the referencing models — and a deferred transaction that upgrades to a
write that late loses to any writer that committed in between. SQLite answers
`database is locked` and no busy timeout retries it. Sessions journal constantly, so
saving settings while one is working is exactly that race.

## Web UI

`clients/web/src/api/client.ts` loses `config.update` and gains `config.models.*` and
`config.modelProviders.*`, plus the moved ChatGPT sign-in calls. The Settings page
currently edits a local copy of the whole settings
document and saves it in one shot; it changes to saving each added, edited or removed
row through its own request.

This is a real behaviour change worth stating: a partial failure is now possible, where
three of four edits land and the fourth returns `422`. The page reports per-row status
rather than one save-succeeded banner, which is more honest than the current shape —
today a single invalid row rejects the entire save including the valid rows.

## Testing

- **Store unit tests** per operation, in `crates/server/src/config/store.rs` beside the
  existing ones: upsert-creates, upsert-updates, omitted-key-preserved, `""`-clears-key,
  chatgpt-kind-drops-key, kind-change-clears-oauth, delete-clears-oauth,
  delete-blocked-by-referencing-model, model-rejects-unknown-provider.
- **Isolation tests** proving each new route is user-scoped, matching the existing
  scope-audit harness. The CI scope audit already fails on a table read without a
  `user_id` predicate, so the new queries inherit that guard.
- **HTTP tests** in `crates/server/src/http/mod.rs` for the status codes that carry
  meaning: `409` on a referenced provider, `422` on path/body identity mismatch, `404`
  on a missing row, `204` on delete.
- **A concurrency test** is the point of the whole change: two concurrent upserts of
  *different* providers must both survive. Under `PUT /api/config` one would lose.

## Out of Scope

- Renaming a provider or model. Delete and recreate; the models that reference the old
  name block the delete, which is the correct forcing function.
- Vendors — they are a live roster of connected agents, not stored settings.
- Model cards, which are a prefill catalogue rather than a source of truth.
- The Terraform provider itself, which consumes this API in its own repo.
