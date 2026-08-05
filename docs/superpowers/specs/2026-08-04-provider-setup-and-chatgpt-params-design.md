# Provider setup, end to end

A ChatGPT-plan turn fails with `400 Unsupported parameter: max_output_tokens`.
Tracing why led to three more defects in the same path: the server spends
credentials it was never configured with, a provider keeps the credential it had
before you changed its kind, and the settings UI reports a field that does not
authorize anything. This design fixes all four.

## Verified against the live deployment

Every claim below was checked against the homelab horsie (`ghcr.io/blossomstack/horsie:sha-0d55653`)
and its real ChatGPT credential, not inferred from the source.

`POST https://chatgpt.com/backend-api/codex/responses`, with the body horsie
builds:

| Body | Result |
| --- | --- |
| with `"max_output_tokens": 128000` | `400 {"detail":"Unsupported parameter: max_output_tokens"}` |
| identical, field omitted | `200`, streams normally |
| tools + forced `tool_choice` | `200` |

The settings DB explains the rest. Two providers are `kind = chatgpt` and both
carry a 15-character `api_key`; only one has a `provider_oauth` row:

| name | kind | api_key length | signed in |
| --- | --- | --- | --- |
| `codex` | chatgpt | 15 | yes |
| `test` | chatgpt | 15 | no |

A `chatgpt` provider has no way to acquire a key through the UI — the field is
not rendered for that kind. The key is a leftover from when the provider was a
different kind, retained by `resolve_secret`. Both rows therefore report
`has_inline_key: true`, which is why the settings list says "Key set" for a
provider whose authorization is an OAuth token, and says it just as confidently
for `test`, which cannot authenticate at all.

## 1. Do not send `max_output_tokens` on a ChatGPT credential

`build_body` in `providers/openai-responses/src/lib.rs` always sets the field,
defaulting to `DEFAULT_MAX_TOKENS`. The platform Responses API accepts it; the
Codex backend rejects it outright, and Codex itself never sends it.

Branch on `Credential::ChatGpt`, not on the base URL: the tests point a ChatGPT
credential at a mock server, and they must keep exercising the real branch.

```rust
max_output_tokens: match self.credential {
    Credential::ChatGpt(_) => None,
    _ => self.max_tokens.or(request.max_tokens).or(Some(DEFAULT_MAX_TOKENS)),
},
```

A model's configured max-tokens is silently unused on a plan. That is what the
backend allows, and it matches Codex.

## 2. Credentials come from the settings store, never the process environment

`store.rs` falls through to `AnthropicProvider::new()` / `OpenAiProvider::new()`
/ `ResponsesProvider::new()` whenever a provider row has no stored key. Those
constructors read the process environment: `OPENAI_API_KEY` directly, and
`ANTHROPIC_API_KEY` inside `async-llm`'s client (`async-llm-0.8.0/src/client.rs:84`).

So a provider row with an empty key silently spends whatever key the server
process inherited — a credential the operator never attached to that provider,
under a name that claims to have none. On a server growing per-user scoping,
that is a credential-confusion bug, not a convenience.

A missing or empty key becomes a build error:

```
provider 'x' has no API key — add one in settings
```

`env_base_url()` is the same shape and closed the same way: `OPENAI_BASE_URL` /
`ANTHROPIC_BASE_URL` override a provider whose base URL is unset in settings,
redirecting the request — and the key with it — to a host the settings never
named. The server always passes an explicit base URL: the configured one, or the
crate's documented default. The environment cannot retarget a provider.

Because the registry is rebuilt inside the settings transaction, this also means
a credential-less provider can no longer acquire models — which is what makes
the UI gate in §5 an enforcement rather than a suggestion.

**Operator impact.** Any deployment relying on `ANTHROPIC_API_KEY` /
`OPENAI_API_KEY` in the server's environment stops working until the key is
moved into settings. The homelab deployment stores all three keys inline
already, so it is unaffected.

## 3. A provider does not keep the credential of the kind it used to be

Two leaks across a kind change, both live in the homelab DB:

- `store.rs` resolves a provider's key as `resolve_secret(&p.api_key, keep.get(name))`,
  so switching a provider to `chatgpt` retains the API key it had before.
- The `provider_oauth` sweep only prunes rows whose provider *name* disappeared,
  so switching a provider away from `chatgpt` leaves a live refresh token behind,
  ready for whatever that name becomes next.

Fix both at the write: force `api_key = NULL` when the incoming kind is
`chatgpt`, and prune the `provider_oauth` row of any provider whose new kind is
not `chatgpt`. A provider carries exactly the one credential its kind uses.

## 4. One field: `has_credential`

`ProviderView.has_inline_key` answers "is a string stored in the `api_key`
column", which is not the question anyone is asking. Replace it with
`has_credential: bool` — *this provider can authenticate*:

- `chatgpt` → a `provider_oauth` row exists
- every other kind → `api_key` is non-empty

It is the single field behind the row lamp, the Add-model gate, and the editor's
"•••• stored" placeholder. Computing it costs one `read_provider_oauth` in the
settings view. Once §3 lands, the two definitions can no longer disagree for a
single provider.

## 5. Setting up a provider takes one pass

**The row reports the credential it actually uses.** The lamp reads
`has_credential`; its words follow the kind — "Key set" / "No key" for key
kinds, "Connected" / "Not connected" for `chatgpt`.

**Connect lives on the row.** A `chatgpt` provider with no credential shows a
**Connect** button beside its lamp, which expands the device-code panel under
that row. `ChatGptSignIn` is already self-contained — a `provider` prop and its
own start/poll/sign-out — so it needs a mount point and an `onChanged` callback
to invalidate the settings query; without that the row lamp and the panel
disagree after a successful sign-in. Sign-in no longer routes through the
provider editor.

**Adding a ChatGPT provider does not need a second edit.** Saving a new
`chatgpt` provider leaves its sign-in panel open on the new row. Add → kind →
name → Save → device code, in one pass. The "Save this provider first…" hint
stays on the *unsaved* editor, where it is true.

**Add model is gated on the provider being usable.** `Section` gains
`addDisabled` and `addTitle`; the Models section passes
`addDisabled={!selectedProvider?.hasCredential}` with the reason in the tooltip
and in the empty-state text, so a dead button is never a dead end — "Sign in to
this ChatGPT plan first" / "Add an API key to this provider first". Only *Add*
is gated. Editing and deleting existing models stay available, because a
provider can lose its credential after its models exist.

## Testing

- `openai-responses`: `max_output_tokens` absent under `Credential::ChatGpt`,
  present under an API key.
- `store.rs`: a key-less provider fails to build with the new message; the
  environment is not consulted; a kind change to `chatgpt` drops the API key; a
  kind change away from `chatgpt` drops the OAuth row; `has_credential` is true
  for a signed-in `chatgpt` provider and false for one with only a stale key.
- `clients/web`: a vitest over `ModelsSettings` — a `chatgpt` provider without a
  credential shows Connect and a disabled Add model; with one, Add model is
  enabled.
- Live: rebuild the server image on the homelab host, swap the container in
  place, and run a real turn through `codex` / `gpt-5.6-luna`.

## Filed separately, not fixed here

- **An LLM error never reaches the transcript.** `TurnFailed` is journaled and
  sets `last_error`, so the header banner survives a refresh, but the message
  history has no entry: the `Error` SSE frame is live-only in `useSessionStream`.
  Its natural home is the `HistoryEntry` union that PR #140 introduced for hook
  records, so the error lands at the point in the conversation where it happened.
- **The Codex backend discards `prompt_cache_key`.** Sending `horsie-abc`, a
  valid UUID, or nothing at all each came back echoing a different server-assigned
  UUID. PR #209 added the key to stop a plan's own window paying for the re-sent
  prefix; on this backend the parameter appears to be ignored. Whether caching
  still happens under the server's own key is unverified — the echo alone does
  not prove the prefix is re-read at full price.
