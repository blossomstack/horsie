# Getting started

This walks you from nothing to a working chat session.

## 1. Get the server binary

horsie server is a single binary, `horsie-server`. Choose one:

**Build from source** (needs a recent Rust toolchain), from the repo root:

```bash
make build-server      # builds ./target/release/horsie-server
make install-server    # optional: install it into ~/.local/bin
```

**Or use the container image** (bundles the web UI):

```bash
docker build -f docker/server.Dockerfile -t horsie-server:latest .
```

The image serves the API and the web UI together on one port and stores its data
under `/data` (mount a volume there).

## 2. Write a minimal `config.json`

`config.json` holds only deployment settings. Everything you tune later lives in
the Settings UI, so this file stays tiny. A completely empty file (or no file at
all) is valid and uses sensible defaults.

The server looks for `~/.config/horsie/config.json` (or `$XDG_CONFIG_HOME/horsie/config.json`);
override with `--config <path>`. A starter file:

```jsonc
{
  // Allow a local runtime daemon to register (see step 5). Default: false.
  "local_runtime": true
}
```

That is enough to start. For every available field — storage locations, the
database URL, plugin hook paths — see the [Settings reference](settings-reference.md).

## 3. Start the server

```bash
horsie-server --addr 127.0.0.1:3789
```

- `--addr` — where to listen. Use `0.0.0.0:3789` to accept connections from other
  hosts on your network. Default is `127.0.0.1:3789` (this machine only).
- `--config <path>` — use a specific config file.
- `--web <dir>` — also serve a built web UI from `<dir>` on the same port (see
  below). The container image sets this for you.

Everything runs on this **one HTTP port** — the API, the live event streams, and
the runtime connections. There are no other ports to open.

> **Security:** there is no authentication. Only bind `0.0.0.0` on a trusted
> network, or front the server with your own auth proxy.

## 4. Open the web UI

The UI ships with the server. Point the server at the built assets and open it in
a browser:

```bash
make web-build                              # builds clients/web/dist
horsie-server --addr 0.0.0.0:3789 --web clients/web/dist
```

Now browse to `http://<host>:3789`. (The container image already bundles and
serves the UI, so you just open the published port.)

You should see an empty sessions list with **Settings** and **Skills** in the
sidebar.

## 5. Configure a model, then a runtime

A new server has no models and no runtime, so a session can be created but can't
run yet. Two more steps:

**Add a provider and a model.** Open **Settings**:

1. Under **Providers**, add a provider — a name and an **inline API key** (leave
   the key blank on a later edit to keep the stored one). Add an optional base
   URL for gateways/proxies.
2. Under **Models**, add a model — an alias you'll pick in the UI, the provider,
   and the model id (plus an optional max-tokens).
3. **Save changes.** Provider and model edits take effect on the next turn — no
   restart.

**Make a runtime available.** The quickest option is the `local` runtime: a small
daemon you run **on your own machine** (where your working files are) that dials
back to the server. With `"local_runtime": true` in `config.json` (step 2), run
it, pointing `--endpoint` at the server's address:

```bash
horsie-runtime \
  --endpoint "ws://SERVER-HOST:3789/api/runtime/connect?register=local" \
  --runtime-id local \
  --workspace main=/path/to/your/project
```

It dials the server and registers as a runtime vendor named after `--runtime-id`.
Using `--runtime-id local` matches the server's default, so new sessions use it
automatically. (If you're just trying things out on the same host as the server,
use `127.0.0.1` for `SERVER-HOST`.) Keep this process running. For the full
picture — including the managed **velos** option and what each vendor can do —
see [Runtime vendors](runtime-vendors.md).

## 6. Create your first session

1. Click **New** in the sidebar.
2. Give it an optional **Name** and pick a **Model**.
3. Click **Create**.
4. Type a message and send. Watch the agent stream its reply, call tools, and
   work in the runtime. Press **Stop** to interrupt a run.

That's a working session. From here:

- [Sessions](sessions.md) — everything the chat view and New Session dialog offer.
- [GitHub](github.md) — run sessions against real repositories.
- [MCP servers](mcp-servers.md) and [Skills & plugins](skills-and-plugins.md) —
  give agents more tools and capabilities.

## Troubleshooting

- **"Couldn't load settings / bundles"** in the UI — the browser can't reach the
  server. Confirm `horsie-server` is running and that `--addr` matches the URL
  you opened.
- **A session won't run a turn** — you have no active runtime. Check that a
  runtime daemon is connected (or a velos vendor is configured) and that a model
  is set. See [Runtime vendors](runtime-vendors.md).
- **`403` when the runtime daemon connects** — the shared local runtime is
  disabled. Set `"local_runtime": true` in `config.json` and restart the server.
