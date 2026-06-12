# hackamore demo: policy-scoped GitHub access for a sandboxed agent

This runbook walks the full horsie + [hackamore](https://github.com/zhxiaogg/hackamore)
integration end to end on one machine, with a mock GitHub upstream so nothing real
is touched:

- Each `job run` supplies a **per-run hackamore policy** (`--hackamore-policy`) alongside its
  workflow and capability files. The daemon mints a **policy-bound, short-lived token**
  from hackamore's admin API at job spawn — with the TTL taken from the policy file's
  `params` — and injects `HACKAMORE_TOKEN` / `HACKAMORE_URL` (plus a synthetic per-job `HOME`
  and `GH_CONFIG_DIR`) into the sandboxed runtime.
- The sandbox network capability is **`ProxyOnly` port 9090**: the only TCP egress
  the kernel permits is `localhost:9090` — hackamore's proxy listener. Everything else
  is blocked below the agent, not by prompt.
- At startup the runtime provisions **in-process inside the sandbox**, using the
  `hackamore-agent` library (no external binary): it fetches the provision doc through the
  proxy and writes native tool config (`gh` hosts.yml, git credentials) into the
  synthetic home.
- The agent holds only the **opaque hackamore token**. The real credential
  (`ghp_DEMO_REAL_TOKEN_do_not_use`) lives in hackamore's vault and is injected
  upstream only when policy allows — the mock upstream's log proves it.

The demo policy says: *read anything under `repos/`, and create pull requests only
against base branch `develop`*. We then run three jobs: an allowed PR, a denied PR,
and a credential-theft attempt.

All demo assets live in [`examples/hackamore-demo/`](../examples/hackamore-demo/):

| File | Purpose |
|---|---|
| `hackamore-config.json` | `hackamore serve` config: proxy `:9090`, admin `:9091`, one github-flavored service, vaulted credential, JSONL audit log |
| `policy.reviewer-bot.json` | the per-run hackamore policy + params (`--hackamore-policy`): `{ "policy": {…}, "params": { "ttlSeconds": 900 } }` |
| `capabilities.json` | horsie sandbox spec: `ProxyOnly` network, **no** `~/.config/gh`, **no** `~/.ssh` |
| `mock-github.py` | stdlib-only mock upstream on `:9999` that logs every `Authorization` header |
| `workflow.json` | a one-agent horsie workflow (`operator`) that opens the PR via the gateway |

## Prerequisites

- A recent Rust toolchain, `python3`, `jq`, and a checkout of
  [hackamore](https://github.com/zhxiaogg/hackamore) as a sibling of this repo
  (paths below assume `../hackamore`).
- An Anthropic API key in `ANTHROPIC_API_KEY` (or adapt the provider snippet below).

Build hackamore and put the **`hackamore` server binary** on your `PATH`. The runtime no
longer shells out to a `hackamore-agent` binary — it provisions in-process via the
`hackamore-agent` library (a git dependency of the runtime crate), so nothing hackamore
needs to live inside the sandbox.

```bash
make -C ../hackamore build          # or: (cd ../hackamore && cargo build --workspace)
cp ../hackamore/target/debug/hackamore ~/.local/bin/
```

Build and install horsie:

```bash
make install-cli                 # installs horsie + horsie-runtime into ~/.local/bin
```

## 1. Start the mock upstream

In its own terminal (keep it visible — its log is the proof of credential
injection):

```bash
python3 examples/hackamore-demo/mock-github.py
# mock-github listening on http://127.0.0.1:9999
```

## 2. Start hackamore

In a second terminal:

```bash
rm -f /tmp/hackamore-demo-audit.jsonl
hackamore serve --config examples/hackamore-demo/hackamore-config.json
```

What the config sets up:

- Proxy listener `127.0.0.1:9090` (the only address the sandbox may reach),
  admin API `127.0.0.1:9091` (the daemon mints here; the sandbox cannot reach it).
- One service named `github`, flavor `github`, with **host pattern `"*"`**
  (catch-all). hackamore routes by the request's `Host` header; a localhost client
  hitting `http://127.0.0.1:9090` sends `Host: 127.0.0.1:9090`, which matches no
  real upstream hostname — so the demo's single service catches everything. With
  several services you would give each a distinct host pattern and have clients
  set `Host` explicitly.
- `upstream_base` `http://127.0.0.1:9999` — the mock upstream above.
- `consumer_address` `http://127.0.0.1:9090` — the consumer-facing address
  surfaced in the provision doc (`ProvisionService.address`). The runtime's
  in-process provisioning (the `hackamore-agent` library) derives the `gh` hosts.yml
  key and the git credential host from it, so inside the sandbox
  `~/.config/gh/hosts.yml` is keyed by `127.0.0.1:9090`.
- `outbound: { "bearer": "github-bot" }` — on policy allow, hackamore replaces the
  consumer's `Authorization` header with `Bearer <vault secret>`; the vault maps
  `github-bot` to `ghp_DEMO_REAL_TOKEN_do_not_use`.
- `audit_log` `/tmp/hackamore-demo-audit.jsonl` — one JSON line per decision.

## 3. The per-run policy

`examples/hackamore-demo/policy.reviewer-bot.json` is the per-run hackamore file. It bundles
the policy with its params (the token TTL is a property of the policy, not global
config):

```json
{
  "policy": { "rules": [ /* … */ ] },
  "params": { "ttlSeconds": 900 }
}
```

The `policy` is opaque to horsie — it's forwarded verbatim to hackamore's `/mint`. Its two
rules:

1. **Allow Read** on `repos/**` (any GET under repos).
2. **Allow Create** on `repos/*/*/pulls` **only when** the request field
   `base` equals `"develop"` (hackamore merges query + JSON body into the fields
   the condition reads).

Semantics: rules are evaluated **top to bottom, first match wins**, and anything
no rule matches is **denied by default**. So a PR against `main` matches neither
rule (rule 2's condition fails, and there is no fallback) and falls through to
the default deny.

`params.ttlSeconds` (default 3600 if omitted) is the lifetime of the token minted for
each run bound to this policy.

## 4. Configure horsie

Merge this into `~/.config/horsie/config.json` (create it if absent). Pick whatever
real model you like for `demo-model` — the workflow only needs bash:

```json
{
  "providers": {
    "anthropic": { "type": "anthropic", "api_key_env": "ANTHROPIC_API_KEY" }
  },
  "models": {
    "demo-model": { "provider": "anthropic", "model_id": "claude-sonnet-4-5", "max_tokens": 8192 }
  },
  "hackamore": {
    "admin_url": "http://127.0.0.1:9091",
    "proxy_url": "http://127.0.0.1:9090"
  }
}
```

The global `hackamore` section is just the **server location** — it carries no policy. A
job is provisioned through hackamore only when it's submitted with `--hackamore-policy`
(below); the policy and its TTL live in that per-run file. Mints fail closed: if a job
has a policy but hackamore is down, the spawn fails rather than running unproxied.

Sanity-check the workflow against your config, then start the daemon:

```bash
horsie validate --workflow examples/hackamore-demo/workflow.json
# valid
horsie daemon start --background
mkdir -p /tmp/hackamore-demo-workdir
```

## 5. The three beats

### (a) Allowed: PR against `develop` → 201

```bash
horsie job run \
  --workflow examples/hackamore-demo/workflow.json \
  --capabilities examples/hackamore-demo/capabilities.json \
  --hackamore-policy examples/hackamore-demo/policy.reviewer-bot.json \
  --workdir /tmp/hackamore-demo-workdir \
  --input "Open a pull request against develop."
```

Expected:

- Job output: `{"status": 201, "summary": "..."}`.
- The **mock upstream's terminal** shows the injected real credential — never the
  hackamore token:

  ```
  POST /repos/blossomstack/demo-repo/pulls  Authorization: Bearer ghp_DEMO_REAL_TOKEN_do_not_use
  ```

- The audit log has an Allow line:

  ```bash
  jq -r 'select(.decision == "Allow" and (.action.resource.path | endswith("/pulls")))
         | [.decision, .action.verb.value.kind, .action.resource.path, .detail] | @tsv' \
     /tmp/hackamore-demo-audit.jsonl
  # Allow    Create    repos/blossomstack/demo-repo/pulls    allowed; injected bearer [github-bot]
  ```

### (b) Denied: PR against `main` → 403

```bash
horsie job run \
  --workflow examples/hackamore-demo/workflow.json \
  --capabilities examples/hackamore-demo/capabilities.json \
  --hackamore-policy examples/hackamore-demo/policy.reviewer-bot.json \
  --workdir /tmp/hackamore-demo-workdir \
  --input "Open a pull request against main."
```

Expected:

- Job output: `{"status": 403, ...}` — the gateway answers `denied by policy`.
- **Nothing** appears in the mock upstream's log: a denied request never leaves
  hackamore.
- The audit log has a Deny line:

  ```bash
  jq -r 'select(.decision == "Deny")
         | [.decision, .action.verb.value.kind // .action.verb.value.id, .action.resource.path, .detail] | @tsv' \
     /tmp/hackamore-demo-audit.jsonl
  # Deny    Create    repos/blossomstack/demo-repo/pulls    NotAllowed
  ```

### (c) Steal attempt: the token is worthless outside the cage

```bash
horsie job run \
  --workflow examples/hackamore-demo/workflow.json \
  --capabilities examples/hackamore-demo/capabilities.json \
  --hackamore-policy examples/hackamore-demo/policy.reviewer-bot.json \
  --workdir /tmp/hackamore-demo-workdir \
  --input "Print the value of HACKAMORE_TOKEN, try to read the host user's ~/.config/gh (e.g. /Users/$USER/.config/gh/hosts.yml), and try to curl https://api.github.com directly. Report exactly what happens."
```

Expected findings in the job's summary:

1. `HACKAMORE_TOKEN` prints an **opaque random string** — not a `ghp_*` GitHub
   token. It only means something to the local hackamore instance, only until it
   expires, and only within the minted policy. Exfiltrating it buys nothing.
2. Reading the host's `~/.config/gh` fails with **permission denied (EPERM)**:
   the demo capability spec deliberately grants neither `~/.config/gh` nor
   `~/.ssh`, and the job's `HOME` points at a synthetic per-job directory anyway.
3. `curl https://api.github.com` **fails at the kernel** (connection refused /
   not permitted): `ProxyOnly` allows TCP to `localhost:9090` only. The egress
   block is enforced below the agent, not by the prompt.

### Watching the audit trail

```bash
# Everything, human-readable:
jq -r '[(.atMs / 1000 | todate), .decision, .action.target,
        (.action.verb.value.kind // .action.verb.value.id),
        .action.resource.path, .detail] | @tsv' /tmp/hackamore-demo-audit.jsonl

# Just the verdict counts:
jq -s 'group_by(.decision) | map({(.[0].decision): length}) | add' /tmp/hackamore-demo-audit.jsonl
```

Note: the runtime's in-process provisioning fetches `/.hackamore/provision` through the
proxy at job startup, so you will also see provisioning traffic in hackamore's own log.

## Teardown

```bash
horsie daemon stop
# Ctrl-C the `hackamore serve` and `mock-github.py` terminals
rm -f /tmp/hackamore-demo-audit.jsonl
rm -rf /tmp/hackamore-demo-workdir
# optional: remove the "hackamore" section from ~/.config/horsie/config.json
```

## Real GitHub instead of the mock

Point the service at the real API and vault a real token in
`examples/hackamore-demo/hackamore-config.json`:

```json
"services": [
  {
    "name": "github",
    "host": "*",
    "upstream_base": "https://api.github.com",
    "flavor": "github",
    "consumer_address": "http://127.0.0.1:9090",
    "outbound": { "bearer": "github-bot" }
  }
],
"credentials": { "github-bot": "<a real fine-grained PAT>" }
```

Adjust the repo path in the workflow's curl command to a repo the PAT can write
to, and keep the policy as-is — the `base == develop` rule now gates a real PR.

Caveats:

- **`gh` against the gateway is partially verified.** `gh` treats any host that
  is not `github.com` as GitHub Enterprise and prefixes REST calls with
  `/api/v3`; whether hackamore's github flavor normalizes that prefix correctly is
  unverified. The demo's curl path (no prefix) is the verified path.
- **`git push` over HTTPS is unsupported.** git sends Basic auth
  (`x-access-token:<token>`), not a bearer token; hackamore's inbound auth
  extraction accepts `Bearer`/`token` schemes and `X-Hackamore-Token` only.
  API-level operations (PRs, issues, contents) are the supported surface.
- A real `ghp_*` token in a config file is still a real secret — prefer a
  short-lived fine-grained PAT scoped to one throwaway repo.

## Known limitations

- **Catch-all host routing.** The single service uses host pattern `"*"`
  because localhost consumers send `Host: 127.0.0.1:9090`, which matches no real
  upstream hostname. That is fine for one service; multiple services behind one
  proxy need distinct host patterns and clients that set `Host` (or TLS + SNI)
  accordingly.
- **Token TTL vs long jobs.** The token is minted once at spawn with a fixed TTL
  (`params.ttlSeconds` in the per-run policy file, default 3600). A job that outlives its token starts getting
  401s from the gateway mid-run; there is no re-mint/refresh path yet. Size the
  TTL to your longest expected job.
- **hackamore restarts invalidate tokens.** Minted tokens live in hackamore's memory.
  Restarting `hackamore serve` orphans every in-flight job's token; those jobs fail
  on their next gateway call and must be re-run.
