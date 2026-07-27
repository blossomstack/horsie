# `horsie session tail` — stream session messages to local JSONL

Date: 2026-07-27
Status: approved (design)

## Goal

Add a CLI command that streams a session's messages from `horsie-server` to a
local JSONL file, for offline analysis, archival, and piping into other tools.
No authentication in scope.

## Surface

New subcommand group `horsie session` (leaves room for future session
commands), first action `tail`:

```
horsie session tail <session-id> --output <path> [--server <url>] [--events messages|all]
```

- `<session-id>`: session UUID — the server's canonical key.
- `--server`: base URL of the session server. Default `http://127.0.0.1:3789`
  (matches `horsie-server`'s default bind).
- `--output`: if an **existing directory**, writes `<session-id>.jsonl` inside
  it; otherwise treated as a file path (parent directories created).
- `--events`: `messages` (default) writes only complete transcript messages
  (`SessionEvent::Message`); `all` writes every event type (deltas, tool
  starts/results, status changes, errors, progression, …).

## Mechanism: pure SSE tail

One mechanism for backfill, live tail, and reconnect: `GET
/api/sessions/:id/events`.

- The server already replays journaled (durable) history after the
  `Last-Event-ID` cursor, then bridges to the live broadcast.
- Resume: on startup, if the output file exists, scan it backwards for the
  last line with a non-null `seq` and send that value as `Last-Event-ID`.
  The server replays strictly after it → re-runs are idempotent, output is
  append-only.
- On connection drop: brief backoff, reconnect with the updated cursor, until
  Ctrl-C. SIGINT → flush and exit 0.
- Unknown session id (404 from `Subscribe`) → clean CLI error, non-zero exit.

Rejected alternatives: backfill via `/history` + `?live=1` (two code paths,
fuzzy resume), daemon-side export (wrong layer — the daemon owns jobs, not
sessions).

## Wire format (JSONL)

Each line is a self-describing envelope so resume works in both modes:

```json
{"seq": 42, "event": {"type": "Message", "message": { ... }}}
```

- `seq`: the SSE event id (agent-journal sequence number). Ephemeral events
  (Delta, ToolStart, StatusChanged, Error, Progressed) carry no SSE id →
  `"seq": null`.
- `event`: the `horsie_models::session::SessionEvent` payload, verbatim.
- The envelope is used in `messages` mode too, keeping both modes' files
  interchangeable and resumable.

Known limitation: in `--events all` mode, a few id-less ephemeral events at a
reconnect boundary may be duplicated. Durable (journaled) events never are.

## Structure

- `cli/src/session.rs` (new module):
  - output-path resolution (file vs existing directory),
  - backward scan of an existing file for the last non-null `seq`,
  - the tail loop: connect → replay+live → append lines (buffered, flushed
    per line) → reconnect on drop.
  - Unit tests alongside (`#[cfg(test)] mod tests`).
- `cli/src/main.rs`: `Session { action: SessionAction }` subcommand +
  dispatch arm.
- `cli/src/lib.rs`: expose `session`.
- No new wire types — the CLI deserializes the existing
  `horsie_models::session::SessionEvent`.

Dependencies: `reqwest` (+ `stream` feature) and `reqwest-eventsource` —
both already workspace dependencies, with `reqwest-eventsource` already in
use by `providers/openai` for SSE consumption. The CLI follows that
precedent rather than hand-rolling an SSE parser. The `Last-Event-ID`
cursor is set as a plain request header on the `EventSource` request.

## Error handling

- Server unreachable / mid-stream disconnect → reconnect loop with capped
  backoff; a reconnect attempt log line goes to stderr (stdout stays clean).
- 404 / invalid session id → exit 1 with a clear message.
- Malformed SSE `data:` JSON → skip the line, warn on stderr, keep tailing
  (matches the server's own "log and skip" posture for unserializable events).
- Unwritable output path → fail fast at startup, before connecting.

## Testing

Unit tests in `cli/src/session.rs`:

- output-path resolution: existing dir → `<id>.jsonl` inside; plain file
  path; parent creation.
- last-`seq` scan: empty file → None; file with only null seqs → None; last
  line corrupt JSON → falls back to earlier valid line.
- Envelope serialization matches the documented shape (`seq` + `event`,
  null seq for id-less events).

E2E against a live `horsie-server` remains manual.

Pre-PR checks: `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo fmt --check`, `cargo test --workspace`.
