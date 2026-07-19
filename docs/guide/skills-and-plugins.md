# Skills & plugins

A **skill/plugin bundle** is a package of skills (and optional hooks) that an
agent can use during a session. You install bundles from git repositories once,
then select which ones a session loads.

Bundles are provisioned into the sandbox at session start, so they need a
**provisioning runtime** — the **velos** vendor. The local runtime doesn't
install bundles. See [Runtime vendors](runtime-vendors.md).

## Install a bundle

Open the **Skills** page (from the sidebar footer):

1. Enter a **Git URL** for the bundle repository, and an optional **ref**
   (branch/tag/commit).
2. Install it. The server clones the repo and adds it to your bundle library.

Each installed bundle lists its name, version, skill count, a **hooks** badge if
it ships hooks, and a description.

## Manage bundles

On the Skills page, per bundle:

- **Default for new sessions** — toggle on to pre-select this bundle in the New
  Session dialog. Handy for bundles you almost always want.
- **Update** — re-clone the bundle at its ref to pull in changes.
- **Delete** — remove the bundle from the library.

## Use bundles in a session

In the New Session dialog, under **Advanced**:

1. Turn on **Enable plugins**.
2. Tick the **Skills** bundles to load. Bundles marked *Default for new sessions*
   are pre-checked.

These options appear only when the session's runtime supports provisioning
(velos). At session start, the runtime fetches the selected bundles and makes
their skills available to the agent.

## Notes

- Bundles come from **git** — there's no upload; point the installer at a repo.
- The **local** runtime does not provision bundles, so the Skills options are
  hidden for sessions using it. Use velos to run sessions with skill bundles.
