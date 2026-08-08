# Wiring up environments

An environment answers one question — *where does this run, and what does it run
against?* — and today nothing asks it. The `environments` table, its CRUD
service, its HTTP surface and its two web pages all exist and are entirely
inert: no session, preset, routine or workflow run references one. Meanwhile the
same question is answered four different ways by four different creation paths,
none of which agree.

This is the design for making the environment the one answer, everywhere.

## What the four paths do today

Creating a session takes an optional `vendor` and an optional `repos` list. A
workflow run takes the same pair. An agent preset carries `repos` but
deliberately no vendor — the rule being that where work runs belongs to the
invocation, not to the saved configuration — so invoking one silently resolves
the server's default vendor. A routine carries neither: it inherits its preset's
repos and always runs on the default vendor, because the runner reasons that an
unattended routine with a stale vendor pin fails every interval with nobody
watching.

Four surfaces, three vocabularies, and a fifth concept — the environment row —
that no one can reach. The reusable bundle a user can already sit down and
define is the one thing they cannot then use.

## The shape

`EnvironmentSpec` is a union of the two things a caller can mean:

```
#[type_tag = "type"]
union EnvironmentSpec {
    Runtime(RuntimeEnvironment),
    Named(NamedEnvironment),
}

struct RuntimeEnvironment { vendor: String, repos: Option<Vec<RepoConfig>> }
struct NamedEnvironment  { name: String }
```

`Runtime` is the ad-hoc environment: name a runtime, and — when it can provision
— the repos to check out into it. `Named` is the predefined one, by name.

There is no third variant for the local runtime. `Runtime { vendor: "local" }`
already says it, and a vendor that cannot provision already rejects a non-empty
`repos` at create; a `Local {}` variant would be a second way to say the same
thing, and would hardcode a vendor name into the protocol.

The types live in `environments.fl`, which means `RepoConfig` moves there from
`session_api.fl`. The traffic is about to reverse — `session_api`, `workflow`
and `routines` all need `EnvironmentSpec` — and leaving `RepoConfig` where it is
would make `environments` and `session_api` import each other.

### Every creation path takes one, and it is required

| | before | after |
|---|---|---|
| `CreateSessionRequest` | `vendor?`, `repos?` | `environment` |
| `WorkflowRunRequest` | `vendor?`, `repos?` | `environment` |
| `AgentInvokeRequest` | — | `environment` |
| `RoutineInput` / `RoutineView` | — | `environment` |
| `AgentView` / `AgentPresetInput` | `repos` | *deleted* |

Required rather than optional-with-a-default, on all four. An optional field
whose absence means "the server's default vendor" is a fifth way to answer the
question, and it is the invisible one: a caller who never names an environment
cannot tell from the request that a choice was made on their behalf. The
server's `default_vendor` setting survives as what the web UI seeds a fresh
draft with — a starting value, not a fallback.

A preset loses `repos` outright. What remains on it is agent configuration —
model, skills, MCP servers, memory spaces, thinking effort — and the environment
is supplied by whoever invokes it. That is the rule presets were already written
to, extended to the half of the question they were still answering.

A routine names one too, and cannot omit it. The runner's original reasoning
still holds — a routine whose environment has gone stale fails unattended — but
the failure is already visible: it lands in `last_error` on the routine row,
which the routine page renders. A routine that cannot say where it runs is worse
than one that says something that later breaks.

`SessionSummary` and `SessionDetail` gain `environment: Option<String>`, the
predefined name a session was created from — absent for an ad-hoc one. The
resolved `vendor` and `repos` stay: they are what the session actually got, and
that is what a reader debugging a session needs.

## Resolution happens once, at creation

`build_session_spec` is already the single funnel every creation path goes
through, so resolution goes there. It takes an `EnvironmentSpec` and the
`EnvironmentService`, and resolves:

- `Runtime { vendor, repos }` → `spec.vendor = vendor`; `repos` become
  `git_checkout` provision steps, exactly as they do now.
- `Named { name }` → read the row. An unknown name is `SpecError::Invalid`,
  which the HTTP layer already maps to 422 and the routine runner to a recorded
  `last_error`. The row's `vendor`, `repos`, `env_vars` and `provision` are
  copied into the spec.

Repo checkouts are ordered before the environment's own provision steps. A step
like `make setup` needs the checkout to have happened; the reverse ordering has
no use.

The vendor-connectivity checks stay where they are — `invoke_agent` and the
routine runner each check before creating, `create_session` deliberately does
not and lets the session land in `ProvisioningFailed`, which is retryable. They
now check the *resolved* vendor. Whether a vendor supports provisioning is still
enforced by the vendor at `create()`, not by the builder: the builder has no
vendor registry, and dragging one in to duplicate a check that already exists
would be the wrong trade.

### The spec is a snapshot

`SessionSpec` gains two fields, both `#[serde(default)]` so existing journal
rows load:

- `environment: Option<String>` — the predefined environment this session came
  from, for display and provenance.
- `env_vars: Vec<EnvVarSpec>` — resolved from the environment.

Everything else a `Named` environment contributes lands in fields the spec
already has (`vendor`, `provision`). That is what makes the snapshot work:
`runtime_manager::runtime_spec` re-assembles the vendor-facing spec from
`SessionSpec` on every create *and* every revive, so a session revived a week
later gets what it was created with. Editing an environment never silently
re-points a live session's workspace or moves it to a different vendor — the
same rule a workflow run already applies to its definition.

`env_vars` seed `rt_spec.env` before the minted GitHub token is pushed onto it,
which is the existing path for getting a value into the runtime child. The
environment service rejects, at save, an env-var name in the server's reserved
`HORSIE_*` namespace or equal to the GitHub-token variable, so a user value
cannot shadow a server-injected one.

### Deleting an environment is unconditional

Sessions snapshotted, so they do not care. A routine holds the only durable
reference, and its next run fails with `unknown environment 'x'` recorded in
`last_error`. That is the same failure mode as a routine whose agent preset was
deleted, which the runner already handles by re-resolving every run and failing
visibly. An in-use guard would be a second, inconsistent answer to a question
routines already have one for.

A predefined environment still may not name the `local` vendor. The union is
what makes that coherent: `Runtime` *is* the ad-hoc environment, and the local
runtime is expressible there. A named environment is for the vendor-managed,
provisionable case.

## Migrations

`routines` gains an `environment` column holding the union as JSON. Existing
rows take `{"type":"Runtime","vendor":"local","repos":[]}` — the local runtime
is the common self-hosted default, and a SQL migration cannot read the server's
configured one. `agents` drops `repos`. Both dialects, as always.

## The web UI: one control, three shapes

The consistency requirement is met by there being *one* implementation rather
than several that agree. The existing `PickerSpec` mechanism already renders a
channel as either a bare icon key (the session config bar) or a labelled field
(the agent form), so a single new picker reaches every surface.

`RuntimeChannel { vendor, setVendor }` becomes
`EnvironmentChannel { environment, setEnvironment }` over a draft union
mirroring the wire one. A single **Environment** key replaces today's separate
**Runtime** and **Repos** keys, and its popover is one flat list with headers:

```
┌ Environment ─────────────────────────┐
│ PREDEFINED                           │
│   ci-sandbox        fly · 2 repos  ✓ │
│   docs-box          fly · 1 repo     │
│ RUNTIMES                             │
│   local             default          │
│   fly-pool          provisions       │
│ ─────────────────────────────────────│
│ Repos                                │
│  ☑ org/api    [ref]                  │
│  ☐ org/web                           │
└──────────────────────────────────────┘
```

One list, not two controls: "where does this run" is one decision, answered
once. What follows the divider depends on the selection.

- **A predefined environment** shows its vendor and repos read-only. They are
  part of the definition; changing them means editing the environment.
- **A runtime whose vendor provisions** shows the GitHub-backed repo checklist —
  today's Repos picker, moved inside.
- **A runtime that cannot provision** shows nothing more. There is no workspace
  to check anything out into.

The session config bar therefore goes from
`[Workflow] [Runtime] [Repos] [Skills] [MCP] [Memory] [Model] [Thinking]` to
`[Workflow] [Environment] [Skills] [MCP] [Memory] [Model] [Thinking]`. Repos
were only ever meaningful beside a provisioning vendor; they now live where that
decision is made.

Three other surfaces follow from the same picker:

**The agent preset form** loses its Repos field. A preset is agent
configuration.

**The routine edit page** gains the picker in field shape, required — save is
blocked until an environment is chosen, the same way it is blocked without an
agent preset.

**The locked session row** — the frozen channels on a session that already
exists — replaces its Runtime key with an Environment key, whose readout shows
the environment name when there is one, plus the resolved vendor and repos.
Draft and locked rows keep the identical shape they were redesigned to have.

**The environments edit page** swaps its free-text vendor input for the same
vendor list the picker uses. It is the one place a vendor is still typed by
hand, and a typo there produces an environment that fails only at invoke.

## The CLI

`horsie workflow run`'s `--vendor` / `--repo` pair becomes `--environment <name>`
*or* `--vendor <v> [--repo …]`, mutually exclusive, one required — the two
variants of the union, as two flag shapes. `horsie agent invoke` and the routine
commands gain the same pair. `horsie agent create` drops `--repo`.

## Testing

Unit tests on the builder cover both variants, an unknown name, the ordering of
repo checkouts against an environment's own provision steps, and env-var
passthrough into the runtime spec. The environment service gains tests for the
reserved-name rejection.

The session, workflow and routine e2e suites move to the new request shape; the
routine suite gains a run whose environment names a deleted row and asserts the
recorded `last_error`.

On the web, vitest covers the picker's three selection cases and the four pages
that render it, and the Playwright suite is updated wherever it currently sets a
runtime or picks repos.
