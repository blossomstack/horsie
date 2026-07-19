# @horsie/web

A modern web UI for the horsie server — create sessions, drive agents in
sandboxed runtimes, and watch turns stream live over SSE.

Built with **Bun + Vite + React 19 + Tailwind v4**, talking to the server over
the [fluorite](https://github.com/zhxiaogg/fluorite)-generated protocol types.

## Stack

| Concern        | Choice                                             |
| -------------- | -------------------------------------------------- |
| Runtime / pkg  | [Bun](https://bun.sh)                              |
| Build / dev    | Vite                                               |
| UI             | React 19, React Router 7                           |
| Styling        | Tailwind v4 (`@tailwindcss/vite`), oklch tokens    |
| Server state   | TanStack Query                                     |
| Live updates   | native `EventSource` (SSE)                          |
| Markdown       | react-markdown + remark-gfm + rehype-highlight (lazy) |
| Protocol types | fluorite → `src/generated` (`@horsie/types` schemas) |

## Prerequisites

- [Bun](https://bun.sh)
- The [`fluorite`](https://github.com/zhxiaogg/fluorite) CLI on `PATH`
  (`cargo install fluorite`) — only needed to regenerate protocol types.
- A running horsie server:

  ```bash
  # from the repo root
  make build-server                    # builds `horsie-server`
  ./target/release/horsie-server       # listens on 127.0.0.1:3789
  ```

  The server needs at least one model provider configured in
  `~/.config/horsie/config.json` (see the main horsie docs).

## Develop

```bash
cd clients/web
bun install
bun run generate-types   # (re)generate TS types from ../../models/fluorite/*.fl
bun run dev              # http://localhost:5299
```

The dev server proxies the whole `/api` surface — REST plus both SSE streams
(`/api/sessions/:id/events`, `/api/events`) — to `http://127.0.0.1:3789`, so
there is no CORS to configure. Point it elsewhere with `HORSIE_SERVER`:

```bash
HORSIE_SERVER=http://127.0.0.1:4000 bun run dev
```

## Build

```bash
bun run typecheck
bun run build     # emits static assets to dist/
```

## Serve from the horsie server (single origin)

For a network-reachable deploy without a separate dev server, point
`horsie-server` at the built assets with `--web`; it serves them same-origin
alongside the API (unmatched non-`/api` paths fall back to `index.html`, so
client-side routes survive a hard refresh) and needs no CORS:

```bash
make web-build                                     # builds clients/web/dist
./target/release/horsie-server \
  --addr 0.0.0.0:3789 \
  --web clients/web/dist                           # UI + API on http://<host>:3789
```

`0.0.0.0` accepts connections from other hosts on the network. There is no auth
in front of the server, so only bind it on a trusted network.

## Layout

```
src/
  api/
    client.ts        # request<T> wrapper + typed `api` surface
    types.ts         # re-export of the fluorite-generated protocol types
  generated/         # fluorite output — do not edit by hand
  hooks/
    useSessions.ts     # TanStack Query list/detail + global SSE feed
    useSessionStream.ts# folds a session's SSE stream into a render model
    useTheme.ts
  components/        # Sidebar, Composer, Transcript, ToolCallCard, …
  pages/            # SessionsLayout (sidebar shell), SessionView, Welcome
```

### How the event stream is consumed

`useSessionStream` opens one `EventSource` per session. The server replays the
durable journal on connect (each frame carries its journal-sequence as the SSE
`id`) and then streams live frames; the browser auto-resumes with `Last-Event-ID`
on reconnect, so the fold is dedup-safe by message id. Ephemeral frames
(`Delta`, `ToolStart`, `StatusChanged`, `Error`) are live-only and never
replayed — matching the server's contract.
