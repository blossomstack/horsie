# Examples

A worked example of driving october end to end: a multi-agent **dev workflow**
that plans, codes, reviews, and opens a pull request, plus the **capability file**
that lets those agents build and push.

## Files

- **`dev-workflow.json`** — a `WorkflowDefinition` with four agents:
  `planner → coder → reviewer → (coder if changes requested | pr)`. The reviewer
  loops back to the coder until it approves, then hands off to the `pr` agent.
  Each agent is scoped to the minimum tool allowlist it needs.
- **`dev-workflow-capabilities.json`** — a `CapabilitySpec` that grants the sandbox
  what the agents need to be useful: network, the Rust toolchain (`~/.cargo`,
  `~/.rustup`), and the credentials for pushing + opening a PR (`~/.ssh`, `~/.config/gh`).
  Home-relative paths (`~/…`) are resolved per-user when october loads the file, so
  this file is portable across machines. System directories for both macOS and Linux
  are listed; paths absent on a given host are skipped.

## Prerequisites

1. **A model** named `kimi-code` in your october config
   (`~/.config/october/config.json`) — or edit the `model` fields in the workflow to
   match a model you have configured. For example:

   ```json
   {
     "providers": { "kimi": { "type": "anthropic", "api_key": "<key>", "base_url": "https://api.kimi.com/coding/" } },
     "models": { "kimi-code": { "provider": "kimi", "model_id": "kimi-for-coding", "max_tokens": 32000 } }
   }
   ```

2. **git over SSH** — the `pr` agent pushes with your SSH key, so the workdir's
   `origin` must be an SSH remote (`git@github.com:owner/repo.git`) and your key must
   be set up (`ssh -T git@github.com` should succeed). A passphrase-protected key
   needs an ssh-agent reachable from the sandbox; an unencrypted key works directly.

3. **gh** — the `pr` agent opens the PR with the GitHub CLI, so `gh auth login` must
   already be done on your machine.

## Run

```bash
october run \
  --workflow examples/dev-workflow.json \
  --capabilities examples/dev-workflow-capabilities.json \
  --workdir /path/to/a/checkout \
  --input "Add a --version flag to the CLI."
```

`--workdir` should be a git checkout whose `origin` is the SSH remote you want the PR
opened against. The agents work on a fresh `october/<sha>` branch; only the final
`pr` agent pushes and opens the PR.

## Security note

This capability file is permissive by design — it hands the sandboxed agents your SSH
key (read) and gh token, so they can push and open PRs on your behalf. Grant it only
for repos and tasks you're comfortable letting an agent act on. To narrow the blast
radius, point `origin` at a single repo and review the branch the workflow produces
before merging.
