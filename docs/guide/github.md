# GitHub integration

Connect a GitHub App once, and you can launch sessions against real repositories:
the runtime checks out the repos you pick, using a short-lived, repo-scoped token
minted per session. The token is never stored and never sent to the browser.

Repository checkout requires a **provisioning runtime** — that means the **velos**
vendor. The local runtime can't clone repos. See
[Runtime vendors](runtime-vendors.md).

## 1. Create a GitHub App

In your GitHub org or account settings, create a GitHub App with:

- **Repository permissions → Contents: Read-only** (enough to clone).

Note its **Client ID**, generate a **Client secret**, note the **App ID**, and
generate a **private key** (you'll get a `.pem`). Install the App on the
repositories (or org) you want sessions to access.

## 2. Connect it in Settings

Open **Settings → GitHub** and fill in:

- **Client ID**
- **Client secret**
- **App ID**
- **Private key** — paste the raw PEM, or a base64-encoded version of it.

Save the App configuration. The **Connect GitHub** button stays disabled until
the App config is saved — once it is, click **Connect GitHub** to run the OAuth
flow. When it completes, the page shows **Connected as @your-login**.

To unlink, click **Disconnect**.

## 3. Launch a session against repos

With GitHub connected **and** a provisioning (velos) vendor selected, the New
Session dialog shows a **Repositories** picker:

1. Choose one or more repositories (use the filter to narrow the list; **Refresh**
   to re-pull the list from GitHub).
2. Optionally set a **ref** (branch/tag/commit) per repo — defaults to the
   repository's default branch.
3. Create the session. The runtime checks out each repo before the agent starts,
   and the session shows repo chips for what's checked out.

If you don't see the picker, confirm both conditions: GitHub is connected, and the
session's runtime vendor supports provisioning.

## GitHub tools (MCP) — optional

Once GitHub is connected, Settings → GitHub also offers a **GitHub tools (MCP)**
toggle. Enabling it adds a `github` MCP server that reuses your existing GitHub
connection for auth — no separate credentials — so agents can use GitHub tools
(issues, PRs, etc.) in addition to a plain checkout. Enable it per session like
any other MCP server. See [MCP servers](mcp-servers.md).

## Notes

- The installation token used for checkout is minted just-in-time, scoped to the
  selected repos, and never persisted or exposed to the browser.
- Read-only Contents permission is enough for checkout. The GitHub tools (MCP)
  integration uses the same connection; grant broader App permissions only if you
  want those tools to do more than read.
