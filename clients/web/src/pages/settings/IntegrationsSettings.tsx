import { Boxes, GitBranch, Loader2, Plus, Server } from "lucide-react";
import { useEffect, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { api, ApiRequestError } from "../../api/client";
import type {
  McpServerInput,
  McpServerView,
  SettingsView,
} from "../../api/types";
import { useGithubDisconnect, useGithubStatus } from "../../hooks/useGithub";
import {
  useConnectMcpServer,
  useDeleteMcpServer,
  useMcpServers,
  useTestMcpServer,
  useUpsertMcpServer,
} from "../../hooks/useMcp";
import { useSettings } from "../../hooks/useSettings";
import { askConfirm } from "../../lib/confirm";
import { ReadError } from "../../components/ReadError";
import { RowLabel, RowShell, TextField, SettingsPage } from "./fields";

/** The remote GitHub MCP endpoint reused via the GitHub App connection. */
const GITHUB_MCP_URL = "https://api.githubcopilot.com/mcp/";
/** Row name of the GitHub MCP server (managed from the GitHub section). */
const GITHUB_MCP_NAME = "github";

/**
 * Outbound connections and build info. Every section here saves itself, so the
 * page has no Save button.
 */
export function IntegrationsSettings() {
  const { data: settings, isLoading, isError } = useSettings();
  return (
    <SettingsPage
        title="Integrations"
        desc="GitHub, MCP servers, and this server's build info."
    >
          {isLoading && (
            <div className="py-16 text-center text-sm text-faint">Loading…</div>
          )}
          {isError && (
            <div className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
              Couldn’t load settings. Is <code>horsie serve</code> running?
            </div>
          )}

          <GithubSection />

          <McpSection />

          {settings && <ServerInfoCard view={settings} />}
      </SettingsPage>
  );
}

/**
 * Connecting *your* GitHub account.
 *
 * The App's own registration — client id, secret, app id, private key — moved
 * to Admin → GitHub App. Those are set once by whoever runs the server; this
 * is the button everyone else came here to press.
 */
function GithubSection() {
  const { data: status } = useGithubStatus();
  const disconnect = useGithubDisconnect();
  const [params, setParams] = useSearchParams();
  const [error, setError] = useState<string | null>(null);

  // Surface the OAuth callback outcome, then strip the params from the URL.
  const connected = params.get("github_connected");
  const oauthError = params.get("github_error");
  useEffect(() => {
    if (connected || oauthError) {
      const next = new URLSearchParams(params);
      next.delete("github_connected");
      next.delete("github_error");
      setParams(next, { replace: true });
      if (oauthError) setError(oauthError);
    }
  }, [connected, oauthError, params, setParams]);

  return (
    <section className="section">
      <div className="mb-3 flex items-start gap-2">
        <GitBranch size={15} className="mt-0.5 text-faint" />
        <div>
          <h2 className="section-title">GitHub</h2>
          <p className="mt-1.5 max-w-prose text-xs leading-relaxed text-faint">
            Connect your GitHub account so sessions can clone your repositories.
          </p>
        </div>
      </div>

      <div className="space-y-3">
        {status?.connected ? (
          <div className="flex items-center justify-between rounded-[var(--radius-control)] px-3 py-2 text-sm">
            <span>
              Connected as <span className="font-mono">@{status.login}</span>
            </span>
            <button
              className="key key-flat text-red-ink"
              onClick={() => disconnect.mutate()}
            >
              Disconnect
            </button>
          </div>
        ) : (
          <div className="flex items-center justify-between gap-3 screen px-3 py-2 text-sm text-dim">
            <span>
              {status?.appConfigured ? (
                "App configured — connect your account."
              ) : (
                <>
                  No GitHub App is registered on this server yet. Set one up in{" "}
                  <Link
                    to="/admin/github-app"
                    className="text-live-ink underline underline-offset-2"
                  >
                    Admin → GitHub App
                  </Link>
                  .
                </>
              )}
            </span>
            <a
              className="key shrink-0 aria-disabled:pointer-events-none aria-disabled:opacity-40"
              href={api.github.authUrl()}
              aria-disabled={!status?.appConfigured}
              title={
                status?.appConfigured
                  ? undefined
                  : "Register the GitHub App in Admin first"
              }
              onClick={(e) => {
                if (!status?.appConfigured) e.preventDefault();
              }}
            >
              Connect GitHub
            </a>
          </div>
        )}

        {status?.connected && <GithubMcpToggle />}

        {error && (
          <div className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
            {error}
          </div>
        )}
      </div>
    </section>
  );
}

/**
 * "Enable GitHub tools (MCP)" — upserts the `github` MCP server (`github_app`
 * auth, reusing the App connection) and smoke-tests it; Disable deletes it.
 * Rendered inside the GitHub section once an account is connected.
 */
function GithubMcpToggle() {
  const { data: servers } = useMcpServers();
  const upsert = useUpsertMcpServer();
  const del = useDeleteMcpServer();
  const test = useTestMcpServer();
  const [error, setError] = useState<string | null>(null);
  const gh = (servers ?? []).find((s) => s.name === GITHUB_MCP_NAME);
  const busy = upsert.isPending || test.isPending;

  const enable = async () => {
    setError(null);
    try {
      await upsert.mutateAsync({
        name: GITHUB_MCP_NAME,
        body: {
          name: GITHUB_MCP_NAME,
          url: GITHUB_MCP_URL,
          auth: { kind: "GithubApp", value: {} },
        },
      });
      const r = await test.mutateAsync(GITHUB_MCP_NAME);
      if (!r.ok && r.error) setError(r.error);
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Failed to enable.");
    }
  };

  const retest = async () => {
    setError(null);
    try {
      const r = await test.mutateAsync(GITHUB_MCP_NAME);
      if (!r.ok && r.error) setError(r.error);
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Test failed.");
    }
  };

  return (
    <div
      className="rounded-[var(--radius-control)] px-3 py-2.5"
      style={{ background: "var(--panel-raised)" }}
    >
      <div className="flex items-center justify-between gap-2">
        <div>
          <p className="text-sm font-medium text-legend">GitHub tools (MCP)</p>
          <p className="mt-1.5 max-w-prose text-xs leading-relaxed text-faint">
            Let sessions call the GitHub MCP server (create PRs, search issues…)
            using this connection.
          </p>
        </div>
        {gh ? (
          <button
            className="key key-flat text-red-ink"
            onClick={() => del.mutate(GITHUB_MCP_NAME)}
          >
            Disable
          </button>
        ) : (
          <button className="key" onClick={enable} disabled={busy}>
            {busy ? <Loader2 size={14} className="animate-spin" /> : null} Enable
          </button>
        )}
      </div>
      {gh && (
        <div className="mt-2 flex flex-wrap items-center gap-2 text-xs">
          {gh.enabled ? (
            <span className="chip !py-0 text-[0.625rem] text-lamp-ok">
              enabled · {gh.toolCount ?? 0} tools
            </span>
          ) : (
            <span className="chip !py-0 text-[0.625rem] text-faint">not tested</span>
          )}
          {gh.lastError && (
            <span className="truncate text-red-ink" title={gh.lastError}>
              {gh.lastError}
            </span>
          )}
          <button
            className="key key-flat ml-auto"
            onClick={retest}
            disabled={busy}
          >
            Test
          </button>
        </div>
      )}
      {error && (
        <div className="mt-2 rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
          {error}
        </div>
      )}
    </div>
  );
}

/**
 * Configured remote MCP servers (generic `none`/`bearer` auth). Self-contained —
 * each row upserts/tests/deletes against `/api/mcp/servers`, independent of the
 * page Save button. The GitHub MCP server (`github_app`) is managed from the
 * GitHub section, so it is excluded here.
 */
function McpSection() {
  const { data: servers, isError, error: loadError } = useMcpServers();
  const [adding, setAdding] = useState(false);
  const generic = (servers ?? []).filter((s) => s.auth.kind !== "GithubApp");

  // Surface the OAuth-callback outcome, then strip the params from the URL.
  const [params, setParams] = useSearchParams();
  const [banner, setBanner] = useState<{ ok: boolean; text: string } | null>(
    null,
  );
  useEffect(() => {
    const ok = params.get("mcp_connected");
    const err = params.get("mcp_error");
    if (ok || err) {
      setBanner(
        ok ? { ok: true, text: `Connected ${ok}.` } : { ok: false, text: err ?? "" },
      );
      const next = new URLSearchParams(params);
      next.delete("mcp_connected");
      next.delete("mcp_error");
      setParams(next, { replace: true });
    }
  }, [params, setParams]);

  return (
    <section className="section">
      {banner && (
        <div
          className={`mb-3 rounded-[var(--radius-control)] border px-3 py-2 text-sm ${banner.ok ? "border-lamp-ok bg-lamp-ok-quiet text-lamp-ok" : "border-red bg-red-quiet text-red-ink"}`}
        >
          {banner.text}
        </div>
      )}
      <div className="mb-3 flex items-start justify-between gap-3">
        <div className="flex items-start gap-2">
          <Boxes size={15} className="mt-0.5 text-faint" />
          <div>
            <h2 className="section-title">MCP servers</h2>
            <p className="mt-1.5 max-w-prose text-xs leading-relaxed text-faint">
              Remote Model Context Protocol servers. Sessions pick which to use;
              their tools appear as <code>mcp__&lt;name&gt;__&lt;tool&gt;</code>.
            </p>
          </div>
        </div>
        <button
          className="key shrink-0 !px-2.5 !py-1.5 text-xs"
          onClick={() => setAdding(true)}
        >
          <Plus size={14} /> Add server
        </button>
      </div>
      <div className="space-y-2.5">
        {isError && (
          <ReadError
            what="MCP servers"
            error={loadError}
            testId="mcp-servers-error"
          />
        )}
        {!isError && generic.length === 0 && !adding && (
          <p className="screen px-3 py-4 text-center text-sm text-faint">
            No MCP servers configured.
          </p>
        )}
        {adding && <McpServerRow onDone={() => setAdding(false)} />}
        {generic.map((s) => (
          <McpServerRow key={s.name} server={s} />
        ))}
      </div>
    </section>
  );
}

/**
 * One MCP server row for both a new (unsaved) and an existing server. Holds a
 * local draft; Save upserts, Test smoke-tests, Remove deletes (or drops the new
 * draft). The name is the id of record, so it is fixed once saved.
 */
function McpServerRow({
  server,
  onDone,
}: {
  server?: McpServerView;
  onDone?: () => void;
}) {
  const upsert = useUpsertMcpServer();
  const del = useDeleteMcpServer();
  const test = useTestMcpServer();
  const connect = useConnectMcpServer();
  const { data: allServers } = useMcpServers();
  const isNew = !server;

  const [name, setName] = useState(server?.name ?? "");
  const [url, setUrl] = useState(server?.url ?? "");
  const [authKind, setAuthKind] = useState<"None" | "Bearer" | "OAuth">(
    server?.auth.kind === "Bearer"
      ? "Bearer"
      : server?.auth.kind === "OAuth"
        ? "OAuth"
        : "None",
  );
  const [tokenInput, setTokenInput] = useState("");
  const [clientId, setClientId] = useState(
    server?.auth.kind === "OAuth" ? (server.auth.value.clientId ?? "") : "",
  );
  const [clientSecret, setClientSecret] = useState("");
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const hasToken =
    server?.auth.kind === "Bearer" ? server.auth.value.hasToken : false;
  const connected =
    server?.auth.kind === "OAuth" ? server.auth.value.connected : false;
  const hasClientSecret =
    server?.auth.kind === "OAuth" ? server.auth.value.hasClientSecret : false;
  const touch = () => setDirty(true);

  const save = async () => {
    setError(null);
    if (!name.trim()) return setError("Name is required.");
    // The name is spliced into every tool id this server contributes
    // (`mcp__<name>__<tool>`), and providers reject an id outside
    // `^[a-zA-Z0-9_-]+$` — so a space or a slash here kills *every* turn in
    // any session using it, with a 400 naming a tool index nobody can map back
    // to a server. The server enforces this too; saying it here means the
    // typing stops before the save.
    if (!/^[a-zA-Z0-9_-]+$/.test(name.trim()))
      return setError(
        "A name may only use letters, digits, '-' and '_' — it becomes part of every tool id this server contributes.",
      );
    if (!url.trim()) return setError("URL is required.");
    // Upsert is right for an edit and wrong for an add: adding under a name
    // already taken silently replaced that server and destroyed its stored
    // credential.
    if (isNew && (allServers ?? []).some((s) => s.name === name.trim()))
      return setError(
        `An MCP server named “${name.trim()}” already exists. Edit it from the list, or pick another name.`,
      );
    const auth: McpServerInput["auth"] =
      authKind === "Bearer"
        ? {
            kind: "Bearer",
            value: { token: tokenInput === "" ? undefined : tokenInput },
          }
        : authKind === "OAuth"
          ? {
              kind: "OAuth",
              value: {
                clientId: clientId.trim() === "" ? undefined : clientId.trim(),
                clientSecret: clientSecret === "" ? undefined : clientSecret,
              },
            }
          : { kind: "None", value: {} };
    try {
      await upsert.mutateAsync({
        name: name.trim(),
        body: { name: name.trim(), url: url.trim(), auth },
      });
      setTokenInput("");
      setClientSecret("");
      setDirty(false);
      onDone?.();
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Failed to save.");
    }
  };

  const runTest = async () => {
    setError(null);
    try {
      const r = await test.mutateAsync(name.trim());
      if (!r.ok && r.error) setError(r.error);
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Test failed.");
    }
  };

  // Discarding an unsaved draft needs no confirm — there is nothing to lose
  // that is not still on screen. Deleting a saved server takes its URL, its
  // auth and any OAuth grant with it, so that one asks.
  const remove = async () => {
    if (isNew) return onDone?.();
    if (!(await askConfirm(`Delete MCP server “${server.name}”?`))) return;
    del.mutate(server.name);
  };

  return (
    <RowShell onRemove={() => void remove()} removeLabel="Remove MCP server">
      <div className="space-y-3">
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          {isNew ? (
            <TextField
              label="Name"
              value={name}
              onChange={(v) => {
                setName(v);
                touch();
              }}
              placeholder="linear"
              hint="Letters, digits, '-' and '_'. It becomes part of every tool id: mcp__<name>__<tool>."
            />
          ) : (
            <div>
              <RowLabel>Name</RowLabel>
              <div className="truncate py-1.5 font-mono text-sm text-legend">
                {name}
              </div>
            </div>
          )}
          <TextField
            label="URL"
            value={url}
            onChange={(v) => {
              setUrl(v);
              touch();
            }}
            placeholder="https://mcp.example.com/"
          />
          <label className="block">
            <RowLabel>Auth</RowLabel>
            <select
              className="field font-mono"
              value={authKind}
              onChange={(e) => {
                setAuthKind(e.target.value as "None" | "Bearer" | "OAuth");
                touch();
              }}
            >
              <option value="None">None (public)</option>
              <option value="Bearer">Bearer token</option>
              <option value="OAuth">OAuth 2.1</option>
            </select>
          </label>
          {authKind === "Bearer" && (
            <TextField
              label="Bearer token"
              type="password"
              value={tokenInput}
              onChange={(v) => {
                setTokenInput(v);
                touch();
              }}
              placeholder={hasToken ? "•••• stored — blank keeps it" : "not set"}
            />
          )}
          {authKind === "OAuth" && (
            <>
              <TextField
                label="Client ID (optional)"
                value={clientId}
                onChange={(v) => {
                  setClientId(v);
                  touch();
                }}
                placeholder="blank = auto-register"
              />
              <TextField
                label="Client secret (optional)"
                type="password"
                value={clientSecret}
                onChange={(v) => {
                  setClientSecret(v);
                  touch();
                }}
                placeholder={
                  hasClientSecret ? "•••• stored — blank keeps it" : "none"
                }
              />
            </>
          )}
        </div>

        {!isNew && (
          <div className="flex flex-wrap items-center gap-2 text-xs">
            {server.enabled ? (
              <span className="chip !py-0 text-[0.625rem] text-lamp-ok">
                enabled · {server.toolCount ?? 0} tools
              </span>
            ) : (
              <span className="chip !py-0 text-[0.625rem] text-faint">not tested</span>
            )}
            {authKind === "OAuth" && connected && (
              <span className="chip !py-0 text-[0.625rem] text-lamp-ok">
                authorized
              </span>
            )}
            {server.lastError && (
              <span className="truncate text-red-ink" title={server.lastError}>
                {server.lastError}
              </span>
            )}
          </div>
        )}

        {error && (
          <div className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
            {error}
          </div>
        )}

        <div className="flex justify-end gap-2">
          {!isNew && authKind === "OAuth" && (
            <button
              className="key"
              disabled={connect.isPending || upsert.isPending}
              onClick={async () => {
                setError(null);
                try {
                  // Persist any client/endpoint edits first so connect uses them.
                  if (dirty) await save();
                  const { url } = await connect.mutateAsync(name.trim());
                  window.location.href = url;
                } catch (e) {
                  setError(
                    e instanceof ApiRequestError ? e.message : "Connect failed.",
                  );
                }
              }}
            >
              {connect.isPending ? (
                <Loader2 size={14} className="animate-spin" />
              ) : null}
              {connected ? "Reauthorize" : "Connect"}
            </button>
          )}
          {!isNew && (
            <button
              className="key"
              onClick={runTest}
              disabled={test.isPending}
            >
              {test.isPending ? (
                <Loader2 size={14} className="animate-spin" />
              ) : null}
              Test
            </button>
          )}
          <button
            className="key key-go"
            onClick={save}
            disabled={(!isNew && !dirty) || upsert.isPending}
          >
            Save
          </button>
        </div>
      </div>
    </RowShell>
  );
}

function ServerInfoCard({ view }: { view: SettingsView }) {
  const { info } = view;
  const rows: [string, string][] = [
    ["Config file", info.configPath || "(none)"],
    ["Database", info.database || "(none)"],
    ["State dir", info.stateDir],
    ["Data dir", info.dataDir],
    ["Plugins dir", info.pluginsDir],
    ["Version", info.version],
  ];
  return (
    <section className="section">
      <div className="flex items-center gap-2">
        <Server size={15} className="text-faint" />
        <h2 className="section-title">Server</h2>
      </div>
      <dl className="mt-3 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
        {rows.map(([k, v]) => (
          <FieldRow key={k} k={k} v={v} />
        ))}
      </dl>
    </section>
  );
}

function FieldRow({ k, v }: { k: string; v: string }) {
  return (
    <>
      <dt className="legend pt-0.5">{k}</dt>
      {/* Selectable, not truncated: these paths exist to be copied into a
          terminal, and an ellipsis makes that impossible. */}
      <dd
        className="min-w-0 font-mono text-[0.6875rem] break-all text-legend select-all"
        title={v}
      >
        {v}
      </dd>
    </>
  );
}
