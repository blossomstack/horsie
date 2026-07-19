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
    "data_dir": "/var/lib/horsie/data",
    // Skill/plugin bundle library. Default: <data_dir>/plugins
    "plugins_dir": "/var/lib/horsie/data/plugins"
  },
  "runtime": {
    // Directories prepended to PATH when running plugin hooks (e.g. the node
    // bin dir). Absent → auto-discover `node` from the environment.
    "hook_path": ["/usr/local/bin"]
  },
  // Allow a local runtime daemon to register itself (see Runtime vendors).
  // Default: false — connections to ?register=local are rejected with 403.
  "local_runtime": true,
  "database": {
    // Where runtime-editable settings are stored. Default: a SQLite file at
    // <data_dir>/server/config.db. Only sqlite:// is supported today.
    "url": "sqlite:///var/lib/horsie/data/server/config.db"
  }
}
```

That's the whole file. Notably, **providers, models, velos vendors, the default
vendor, GitHub, MCP servers, and skill bundles are not here** — they live in the
database and are managed from the UI.

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
| `HORSIE_DATABASE_URL` | Overrides `database.url`. Takes precedence over the config file. |
| `HORSIE_ARTIFACT_SECRET` | Signing secret for the short-lived tokens runtimes use to fetch skill bundles. Unset → a random per-process secret (fine for a single instance). Set a stable value if you run more than one server instance. |

## Settings database (managed in the UI)

Open **Settings**. Sections, top to bottom:

| Section | What you configure |
| --- | --- |
| **Providers** | Model providers: name, optional base URL, inline API key. |
| **Models** | Models you can pick per session: alias, provider, model id, optional max tokens. |
| **Default vendor** | Which runtime vendor new sessions use (only *active* vendors are selectable). Falls back to `local`. |
| **Velos remote runtimes** | Remote runtime vendors: name, server URL, image, advertise address, token, and advanced compute settings. Includes a per-row **Test connection**. See [Runtime vendors](runtime-vendors.md). |
| **GitHub** | GitHub App config + connection; the GitHub-tools-(MCP) toggle. See [GitHub](github.md). |
| **MCP servers** | Remote MCP servers: name, URL, auth. See [MCP servers](mcp-servers.md). |
| **Server** *(read-only)* | Config file path, database, state dir, data dir, plugins dir, version. |

**Skill/plugin bundles** are managed on the separate **Skills** page. See
[Skills & plugins](skills-and-plugins.md).

### When changes take effect

- **Providers / models** — apply to the next turn; no restart.
- **Velos vendors** — most edits apply immediately, but changing the server URL
  or advertise address affects the runtime listener; the UI shows a
  **restart required** banner for those.
- **GitHub, MCP servers, skill bundles** — apply as you save them.

## Data & state on disk

- **`data_dir`** — the durable session journal (transcripts) and the SQLite
  settings database (`<data_dir>/server/`). Back this up; mount a volume here in
  containers.
- **`state_dir`** — ephemeral runtime state; safe to lose across restarts.
- **`plugins_dir`** — the installed skill/plugin bundle library.
