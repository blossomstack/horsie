# Settings reference

The server keeps configuration in two places that never overlap:

- **`config.json`** — deployment/bootstrap settings. Edited by hand; read at
  startup. Small and stable.
- **The settings database** — everything you tune at runtime. Edited from the
  **Settings** page in the UI. Secrets in the database are never returned by the
  API (the UI shows only whether a key is set).

## `config.json` (bootstrap)

Location: `~/.config/horsie/config.json` (or `$XDG_CONFIG_HOME/horsie/config.json`),
or pass `--config <path>`. Every field has a default, so an empty file — or no
file — is valid.

```jsonc
{
  "storage": {
    // Ephemeral runtime state. Default: $XDG_STATE_HOME/horsie or ~/.local/state/horsie
    "state_dir": "/var/lib/horsie/state",
    // Durable session journal + database. Default: $XDG_DATA_HOME/horsie or ~/.local/share/horsie
    "data_dir": "/var/lib/horsie/data"
  },
  "database": {
    // Where runtime-editable settings are stored. Default: a SQLite file at
    // <data_dir>/server/config.db. sqlite:// and postgres:// are both supported.
    "url": "sqlite:///var/lib/horsie/data/server/config.db",
    // Pool size, shared by settings reads and journal writes. Default: 10.
    "max_connections": 10
  },
  "journal": {
    // Where session/agent history is stored: "file" (JSONL under data_dir) or
    // "database" (the journal_* tables in database.url). Default: "database".
    // Switching an existing server from "file" to "database" starts from an
    // empty journal — see Self-hosting.
    "backend": "file"
  },
  "auth": {
    // Require a password for the web UI and API. Default: true. First boot
    // creates an `admin` account and prints a generated password.
    "enabled": true
  },
  // CLI-only: the session server `horsie` commands use when --server is
  // omitted. Managed with `horsie config set default-server`. The server
  // ignores this key.
  "default_server": "https://horsie.example.com"
}
```

That's the whole file. Notably, **providers, models, velos vendors, the default
vendor, GitHub, MCP servers, and skill bundles are not here** — they live in the
database and are managed from the UI. The CLI reads one CLI-owned key,
`default_server`, which the server ignores. Old files that still set the removed
`storage.plugins_dir` or `runtime.hook_path` keys keep parsing — the keys are
ignored (skill bundles are managed from the UI now).

## Command-line flags

`horsie-server` accepts:

| Flag | Default | Purpose |
| --- | --- | --- |
| `--addr <host:port>` | `127.0.0.1:3789` | Bind address. Use `0.0.0.0:3789` for network access. |
| `--config <path>` | `~/.config/horsie/config.json` | Config file to load. |
| `--web <dir>` | *(off)* | Also serve a built web UI from `<dir>` on the same port. |

## Environment variables

| Variable | Effect |
| --- | --- |
| `HORSIE_DATABASE_URL` | Overrides `database.url`. Takes precedence over the config file. Accepts `sqlite://` or `postgres://`. |
| `HORSIE_ARTIFACT_SECRET` | Signing secret for the short-lived tokens runtimes use to fetch skill bundles. Unset → a random per-process secret (fine for a single instance). Set a stable value if you run more than one server instance. |
| `HORSIE_TOKEN` | Bearer token the CLI sends instead of reading `~/.config/horsie/credentials.json`. For scripts and CI. |
| `HORSIE_AUTH_ENABLED` | Overrides `auth.enabled`. `false`/`0`/`no` turns authentication off; `true`/`1`/`yes` turns it on. An unrecognised value falls through to the config file rather than silently disabling it. |

## Settings database (managed in the UI)

Open **Settings**. The left nav lists one page per group of settings:

| Page | Sections | What you configure |
| --- | --- | --- |
| **Models** | Providers | Model providers: name, **kind** (Anthropic, OpenAI-compatible, OpenAI Responses, or ChatGPT plan), optional base URL, inline API key. See [Provider kinds](#provider-kinds). |
| | Models | Models you can pick per session: alias, provider, model id, optional max tokens. |
| **Runtimes** | Default vendor | Which runtime vendor new sessions use (only *active* vendors are selectable). Falls back to `local`. |
| | Connected vendors | Read-only: the vendor agents connected right now and what each announced it can do. Vendors are configured in their own agent process, not here. See [Runtime vendors](runtime-vendors.md). |
| **Skills** | — | Skill/plugin bundles. See [Skills & plugins](skills-and-plugins.md). |
| **Memory** | — | Memory spaces and the notes the agent has saved in them. |
| **Integrations** | GitHub | GitHub App config + connection; the GitHub-tools-(MCP) toggle. See [GitHub](github.md). |
| | MCP servers | Remote MCP servers: name, URL, auth. See [MCP servers](mcp-servers.md). |
| | Server *(read-only)* | Config file path, database, journal backend, state dir, data dir, plugins dir, version. |
| **Appearance** | Theme / Light or dark / Text size / Transcript | How this browser draws horsie: one of four themes, light/dark/system, a three-step text size that scales the whole interface, and the transcript display switches. Stored in the browser, not the settings database, so each browser you use can differ. |
| **Account** | — | Change the admin password and sign out. Shows a notice while the deployment is still using its generated first-boot password. Says so plainly when authentication is disabled. |

**Models** and **Runtimes** batch their edits behind a **Save changes** button —
leaving either page with unsaved edits asks for confirmation first. Every other
page saves each row as you go.

Operator-facing settings live under **Admin**, which has the same layout. Its
only page today is **Model cards**: the catalog of well-known models and token
limits that the Models page autocompletes from.

### Provider kinds

Every provider has a **kind** that selects the wire protocol the server speaks
to it. The same provider fields (name, base URL, inline key) apply to both.

| Kind | Speaks | Use it for |
| --- | --- | --- |
| **Anthropic** | the Anthropic Messages API | Claude models, or any endpoint that speaks the Anthropic wire (set a base URL). |
| **OpenAI-compatible** | `/v1/chat/completions` | OpenAI, plus self-hosted and third-party servers that expose the same API — Ollama, vLLM, llama.cpp, OpenRouter, DeepSeek. |
| **OpenAI Responses** | `/responses` | OpenAI's Responses API with a platform API key. Unlike chat completions it carries reasoning across turns, so a reasoning model keeps its train of thought through a tool loop. |
| **ChatGPT plan** | `/responses` on OpenAI's Codex backend | Spending a ChatGPT subscription instead of API credit. Signs in rather than storing a key — see [ChatGPT plan](#chatgpt-plan). |

**Example — a local Ollama server**: add a provider with kind
**OpenAI-compatible**, base URL `http://127.0.0.1:11434`, and any placeholder
for the inline key (Ollama ignores it), then a model whose model id is a tag you
have pulled (e.g. `qwen2.5`).

**Example — a hosted OpenAI-compatible service**: kind **OpenAI-compatible**,
base URL the service's endpoint, and an inline API key.

Every provider must carry its own credential. horsie does **not** fall back to
`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` in the server's environment, and does not
let `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` redirect a provider whose base URL
is unset here: a provider is exactly what this page says it is, and nothing in
the server's environment can quietly lend it a key or move where that key is
sent. A provider with no credential is rejected when you save a model against
it, and **Add model** is unavailable until it has one.

#### ChatGPT plan

A **ChatGPT plan** provider spends a ChatGPT subscription's Codex allowance
rather than API credit. It has no API key field: you sign in instead.

Add the provider (kind **ChatGPT plan**, no base URL) and save it. Its row
appears with a **Connect** button and the sign-in already open; press **Sign in
with ChatGPT**. horsie shows an eight-character code and a
link to `auth.openai.com/codex/device`. Open that link on any device — your
laptop, your phone — sign in to ChatGPT there and enter the code. horsie polls
until you approve, then stores the credential and refreshes it from then on
without asking again.

The sign-in is deliberately code-based rather than a browser redirect: the
OAuth client belongs to OpenAI and only redirects to `localhost`, so a horsie
reachable at a public domain could never receive the callback. With the device
flow every call runs outbound from the server, which means **no callback URL, no
inbound route, and no reverse-proxy change** — it works the same on a laptop and
on a hosted deployment.

Two things worth knowing:

- **Usage counts against that ChatGPT plan's Codex limits**, not against an API
  bill. When the plan's window is exhausted, turns fail with a rate-limit error
  until it resets.
- **Model ids are the ones the plan offers** (the Codex line), not the platform
  API's. A model the subscription cannot reach fails with the backend's own
  error rather than being silently substituted.
- **A model's max-tokens is ignored on a plan.** The Codex backend rejects the
  parameter outright, so horsie does not send it and the model runs to whatever
  limit the backend applies. The field still applies to every other kind.

horsie identifies itself honestly to OpenAI (as `horsie`, not as Codex) and
sends no client-attestation header, since only OpenAI's own clients can produce
a valid one. This is a personal-use path: sign in with your own plan.

#### DeepSeek

DeepSeek speaks the OpenAI wire, so it needs no special kind:

- **Kind:** OpenAI-compatible
- **Base URL:** `https://api.deepseek.com`
- **Models:** `deepseek-v4-flash`, `deepseek-v4-pro`

Both models ship in the bundled card catalog, so picking the model id in
**Settings → Models** fills in the context window (1,048,576), the generation
cap (393,216) and the thinking configuration for you.

Thinking is on by default and accepts the full effort ladder — `none`,
`minimal`, `low`, `medium`, `high`, `xhigh`, `max` — despite DeepSeek's own
documentation listing only three of them.

One constraint is worth knowing before choosing DeepSeek for sub-agents.
DeepSeek rejects a pinned tool choice while thinking is enabled, answering
`400 Thinking mode does not support this tool_choice`. The model's **Pinned tool
choice disables thinking** setting handles this by turning thinking off for
exactly those requests, and the bundled DeepSeek cards enable it. Because a
forced-handoff agent pins a tool on *every* turn, such an agent runs with
thinking off throughout — so DeepSeek is a weak choice for handoff-style
sub-agents. Ordinary sessions are unaffected.

Two behaviors differ by kind, and are handled automatically:

- **Reasoning / thinking.** Reasoning models surface their thinking differently
  by backend, and horsie shows it the same way it shows Claude's. DeepSeek,
  vLLM started with a reasoning parser, and OpenRouter
  stream a reasoning trace over `/v1/chat/completions`, which horsie displays as
  a thinking block. **Genuine OpenAI** models (the o-series, GPT-5) keep their
  reasoning hidden on chat completions, so on that kind you see the answer but
  not the thinking; use **OpenAI Responses** or **ChatGPT plan** to get
  summaries. On the chat wire the reasoning is shown but never sent back on the
  next turn, since some backends reject that. The Responses kinds are the
  exception: they replay the model's own reasoning in encrypted form, which is
  what lets a reasoning model keep its thread across a tool loop.
- **Streaming is required.** Both kinds stream responses; a backend that cannot
  stream `/v1/chat/completions` is not supported.

If a model's turn fails immediately after you add an OpenAI-compatible provider,
the usual cause is a base URL that does not end at the server root (the server
appends `/v1/chat/completions` itself) or a model id the backend has not loaded.

### When changes take effect

- **Providers / models** — apply to the next turn; no restart.
- **Default vendor** — applies to the next session created. It may name an
  agent that has not connected yet.
- **Vendors themselves** — not editable here at all. Each vendor agent is
  configured where it runs, and appears (or disappears) as it connects.
- **GitHub, MCP servers, skill bundles** — apply as you save them.

## Data & state on disk

- **`data_dir`** — plugin artifacts, plus (with the default SQLite database) the
  settings database and the journal under `<data_dir>/server/`. Back this up;
  mount a volume here in containers. Set `journal.backend` to `file` and the
  session journal lands here as JSONL instead; a PostgreSQL deployment on the
  default `database` journal keeps only plugin artifacts here.
- **`state_dir`** — ephemeral runtime state; safe to lose across restarts.
