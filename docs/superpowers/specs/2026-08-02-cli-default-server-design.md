# CLI default server

Status: approved design, 2026-08-02.

## Context

Every `horsie` command that talks to a session server (`session`, `agent`,
`auth login`, `connect`) requires or defaults `--server`. The clap default is
hard-coded to `http://127.0.0.1:3789`, which is right for a local dev server
and wrong for the hosted product: a user who has never run a server locally
hits a connection error instead of the service they are trying to use.

The CLI already has a user config, `~/.config/horsie/config.json` (same file
the server's `BootConfig` reads), and a per-server credential file,
`~/.config/horsie/credentials.json` written by `horsie auth login`. There is
no `horsie config` command at all today.

## Goals

- Let a user record one server as their default, so `horsie session list`,
  `horsie agent list`, `horsie connect`, etc. work with no `--server` flag.
- Make `horsie auth login` able to mark the server it just authenticated
  against as the default, and make the *first* login auto-default.
- Replace the hard-coded `http://127.0.0.1:3789` fallback with the hosted
  service `https://auth.horsie.dev` when no `--server` and no default is
  configured.
- Preserve the existing config file: it also carries the server's bootstrap
  fields (`database`, `auth`, …), so writes must never clobber unknown keys.

## Non-goals

- Changing the server or its wire surface. This is CLI-only.
- Per-directory or per-project server selection (e.g. `.horsie` project
  files). One machine-wide default.
- Renaming/removing the `--server` flag; it stays, and explicitly passed it
  always wins.
- Multi-account selection beyond the single default.

## Locked decisions

1. **`default_server` lives in the shared `config.json`**, as a new optional
   key on `HorsieConfig`. Not in `credentials.json` (a secrets file is the
   wrong home for a preference) and not in a third file (there is one config
   path and `--config` already targets it).
2. **Writes are read-modify-write on the raw JSON**, never a re-serialize of
   `HorsieConfig`, so the server's fields and any unknown keys survive.
3. **Built-in fallback is `https://auth.horsie.dev`** (the auth+API subdomain;
   the landing page `horsie.dev` stays marketing). Precedence everywhere:
   `--server` flag > `default_server` in config > built-in fallback.
4. **`horsie config` gets a complete little surface**: `set default-server`,
   `get default-server`, `unset default-server`.
5. **`horsie auth login --server X --default`** marks X default; the first
   credential auto-defaults regardless of the flag. The default is never
   silently moved by later logins.
6. **Logout does not clear the default** — a logged-out default is handled
   gracefully by consumers (the flag-less command targets the configured URL
   and the server decides), and logging back in to the same server restores it.

## Section 1 — config storage & resolution

### `HorsieConfig`

`cli/src/config.rs` gains one deserialize-only field:

```rust
#[serde(default)]
pub default_server: Option<String>,
```

`Default` derives as `None`. Existing tests that parse config JSON are
unaffected; a config without the key still resolves to `None`.

### `horsie config` command

New `Command::Config` subcommand in `cli/src/main.rs`:

```
horsie config set default-server <url>   # validate http(s)://, normalize, write
horsie config get default-server         # print the value, or "no default server set"
horsie config unset default-server       # remove the key; no error if absent
```

All three take `--config <path>` like the marketplace/plugin commands, and
`dispatch` rejects any key other than `default-server` with a validation
error ("unknown config key").

Write path (in `config.rs`): load the file as `serde_json::Value` (missing
file → `serde_json::json!({})`), insert/remove the `"default_server"` key,
pretty-print with a trailing newline, `create_dir_all` the parent. No key
ordering guarantees; nothing downstream depends on ordering.

Validation reuses the scheme rule `connect::server_to_endpoint` already
enforces: the value must parse as `http(s)://…` (accepting `ws(s)` is a
non-goal; the stored value is the base URL, schemes translate later). The
value is stored **normalized** via `auth::normalize_server` so
`https://Auth.Horsie.dev/` and `https://auth.horsie.dev` are one entry, and
comparisons elsewhere (e.g. `auth status`'s default marker) match.

### Resolution

```rust
pub const DEFAULT_SERVER: &str = "https://auth.horsie.dev";

/// `--server` flag > configured `default_server` > built-in fallback.
fn resolve_server(flag: Option<String>) -> Result<String, CliError>
```

`resolve_server` returns the flag verbatim if present; else the config's
`default_server` if set; else `DEFAULT_SERVER`. It resolves config via the
existing `HorsieConfig::resolve` honoring `--config`, so a flag-less command
with a config file already picks the file's default. A configured value that
is unparseable was already rejected at `set` time, so the fallback only
applies when the key is absent — no double-validation needed on read.

### Arg changes

Every `--server` clap arg across `session`/`agent`/`auth login`/`connect`
drops `default_value = "http://127.0.0.1:3789"` and becomes
`Option<String>`. `dispatch` resolves each through `resolve_server` once,
before any subcommand logic runs. `connect`'s `--server` similarly becomes
optional and resolved, so the flag-less `horsie connect --workspace .` dials
the default.

Explicit `--server` output is unchanged: the resolved value flows into the
same `ServerClient::new(server)` / `session::tail` / `auth::login` /
`connect::run` calls.

## Section 2 — auth login integration

### `horsie auth login`

New `--default` flag (`#[arg(long)]`, no value). Inside `login()`, before the
credential is inserted:

```rust
let is_default = default_flag || creds.is_empty();
```

- `--default` forces the server to become default (overwriting an existing
  default — user intent is explicit).
- First credential (`creds.is_empty()`) auto-defaults even without the flag.
- Later logins without `--default` leave the default alone.

After each `creds.save(&path)` in `login()` (both the `--token` shortcut and
the device-flow completion):

```rust
if is_default {
    config::set_default_server(server)?;
    println!("{} is now your default server", normalize_server(server));
}
```

`set_default_server` is the read-modify-write from Section 1. It resolves the
config path the same way `HorsieConfig::resolve_path` does (explicit
`--config` if given on the command, else the user config path), so the
default lands in the same file the rest of the CLI reads.

### Logout

No change. A default whose credential is logged out is a valid state; the
next flag-less command targets it and surfaces the server's own answer (e.g.
`not authorized … run horsie auth login`).

### Status display

`horsie auth status` marks the row whose normalized server equals the
configured default with `(default)`:

```
  https://auth.horsie.dev  —  valid for 59m  (default)
```

The default is read via `HorsieConfig::resolve(None)`; a config error while
reading it is not fatal to `auth status` — it prints the list without
markers rather than failing.

## Section 3 — connect, verification, docs

### `horsie connect`

`--server` → `Option<String>`, resolved through `resolve_server`. All
existing behavior (endpoint translation, pre-flight auth check, bundle
base URL) operates on the resolved value.

### Testing

- **config.rs**: `default_server` parses; absent key → `None`; unknown config
  keys rejected by dispatch; `set`/`get`/`unset` round-trip through a temp
  file; read-modify-write preserves unknown keys (a config carrying the
  server's `database`/`auth` fields survives a `set` byte-for-byte on those
  keys); non-`http(s)` values rejected; normalization on write.
- **resolution**: `resolve_server` precedence (flag > config > built-in),
  honoring an explicit `--config`.
- **auth**: first-login auto-default and `--default` overwrite, via a pure
  injectable core (config path + credentials path injected, like the existing
  `resolve_with` / `resolve_token_with` pattern) so no network or real home
  dir is needed; `status` prints `(default)` only on the matching row.
- **Workspace bar** (pre-PR): `cargo test --workspace`,
  `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --check`.
- **No server changes**; `connect_e2e.rs` and all server tests run
  unmodified.

### Docs

- `docs/guide/getting-started.md`: in step 2 (Log in), note the first login
  becomes the default and `--default` marks a later one; add a short "default
  server" paragraph — `horsie config set default-server <url>` overrides,
  `horsie config get/unset` read/clear, and commands without `--server`
  target the default, falling back to `https://auth.horsie.dev`.
- `docs/guide/settings-reference.md`: note the CLI-owned `default_server` key
  in `config.json` alongside the server bootstrap fields (it documents the
  same file).

## Migration and operational impact

None. The new key defaults absent; the built-in fallback changes only the
behavior of commands that previously targeted a hard-coded localhost. Users
who always pass `--server` see no difference. Old config files continue to
parse.
