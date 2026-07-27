import { Boxes, GitBranch, Loader2, Plus, Server } from "lucide-react";
import { useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { api, ApiRequestError } from "../../api/client";
import type {
  McpServerInput,
  McpServerView,
  SettingsView,
} from "../../api/types";
import {
  useGithubAppConfig,
  useGithubDisconnect,
  useGithubStatus,
  useSaveGithubAppConfig,
} from "../../hooks/useGithub";
import {
  useConnectMcpServer,
  useDeleteMcpServer,
  useMcpServers,
  useTestMcpServer,
  useUpsertMcpServer,
} from "../../hooks/useMcp";
import { useSettings } from "../../hooks/useSettings";
import { RowLabel, RowShell, TextField } from "./fields";
import { SettingsHeader } from "./SettingsHeader";

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
    <div className="flex h-full flex-col overflow-hidden">
      <SettingsHeader
        title="Integrations"
        desc="GitHub, MCP servers, and this server's build info."
      />

      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-3xl space-y-6 px-6 py-6">
          {isLoading && (
            <div className="py-16 text-center text-sm text-faint">Loading…</div>
          )}
          {isError && (
            <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
              Couldn’t load settings. Is <code>horsie serve</code> running?
            </div>
          )}

          <GithubSection />

          <McpSection />

          {settings && <ServerInfoCard view={settings} />}
        </div>
      </div>
    </div>
  );
}

/**
 * The GitHub connection settings: App config (write-only secrets),
 * Connect/Disconnect, and the OAuth-callback outcome banner. Self-contained —
 * it saves to `/api/github/app-config`, independent of the page Save button.
 */
function GithubSection() {
  const { data: status } = useGithubStatus();
  const { data: cfg } = useGithubAppConfig();
  const save = useSaveGithubAppConfig();
  const disconnect = useGithubDisconnect();
  const [params, setParams] = useSearchParams();

  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [appId, setAppId] = useState("");
  const [privateKey, setPrivateKey] = useState("");
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Seed the form from the stored config (once, until the user edits it).
  useEffect(() => {
    if (!cfg || dirty) return;
    setClientId(cfg.clientId ?? "");
    setAppId(cfg.appId != null ? String(cfg.appId) : "");
  }, [cfg, dirty]);

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

  const submit = async () => {
    setError(null);
    try {
      await save.mutateAsync({
        clientId: clientId.trim(),
        clientSecret: clientSecret === "" ? undefined : clientSecret,
        appId: appId.trim() === "" ? undefined : Number(appId),
        privateKey: privateKey === "" ? undefined : privateKey,
      });
      setClientSecret("");
      setPrivateKey("");
      setDirty(false);
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Failed to save.");
    }
  };

  return (
    <section className="card p-4">
      <div className="mb-3 flex items-start gap-2">
        <GitBranch size={15} className="mt-0.5 text-faint" />
        <div>
          <h2 className="text-sm font-semibold text-text">GitHub</h2>
          <p className="mt-0.5 text-xs text-faint">
            Connect a GitHub App so sessions can clone your repositories.
          </p>
        </div>
      </div>

      <div className="space-y-3">
        {status?.connected ? (
          <div className="flex items-center justify-between rounded-[var(--radius)] border px-3 py-2 text-sm">
            <span>
              Connected as <span className="font-mono">@{status.login}</span>
            </span>
            <button
              className="btn-ghost text-error"
              onClick={() => disconnect.mutate()}
            >
              Disconnect
            </button>
          </div>
        ) : (
          <div className="flex items-center justify-between rounded-[var(--radius)] border border-dashed px-3 py-2 text-sm text-muted">
            <span>
              {status?.appConfigured
                ? "App configured — connect your account."
                : "Configure the GitHub App below, then connect."}
            </span>
            <a
              className="btn-outline aria-disabled:pointer-events-none aria-disabled:opacity-40"
              href={api.github.authUrl()}
              aria-disabled={!status?.appConfigured}
              title={
                status?.appConfigured
                  ? undefined
                  : "Configure the GitHub App below first"
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

        <div className="grid grid-cols-2 gap-3">
          <TextField
            label="Client ID"
            value={clientId}
            onChange={(v) => {
              setClientId(v);
              setDirty(true);
            }}
          />
          <TextField
            label="Client secret"
            type="password"
            value={clientSecret}
            onChange={(v) => {
              setClientSecret(v);
              setDirty(true);
            }}
            placeholder={
              cfg?.hasClientSecret ? "•••• stored — blank keeps it" : "not set"
            }
          />
          <TextField
            label="App ID"
            value={appId}
            onChange={(v) => {
              setAppId(v);
              setDirty(true);
            }}
          />
          <TextField
            label="Private key (PEM or base64)"
            type="password"
            value={privateKey}
            onChange={(v) => {
              setPrivateKey(v);
              setDirty(true);
            }}
            placeholder={
              cfg?.hasPrivateKey ? "•••• stored — blank keeps it" : "not set"
            }
          />
        </div>

        {error && (
          <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
            {error}
          </div>
        )}

        <div className="flex justify-end">
          <button
            className="btn-primary"
            onClick={submit}
            disabled={!dirty || save.isPending}
          >
            Save GitHub settings
          </button>
        </div>
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
      className="rounded-[var(--radius)] border px-3 py-2.5"
      style={{ background: "var(--surface-2)" }}
    >
      <div className="flex items-center justify-between gap-2">
        <div>
          <p className="text-sm font-medium text-text">GitHub tools (MCP)</p>
          <p className="mt-0.5 text-xs text-faint">
            Let sessions call the GitHub MCP server (create PRs, search issues…)
            using this connection.
          </p>
        </div>
        {gh ? (
          <button
            className="btn-ghost text-error"
            onClick={() => del.mutate(GITHUB_MCP_NAME)}
          >
            Disable
          </button>
        ) : (
          <button className="btn-outline" onClick={enable} disabled={busy}>
            {busy ? <Loader2 size={14} className="animate-spin" /> : null} Enable
          </button>
        )}
      </div>
      {gh && (
        <div className="mt-2 flex flex-wrap items-center gap-2 text-xs">
          {gh.enabled ? (
            <span className="chip !py-0 text-[10px] text-success">
              enabled · {gh.toolCount ?? 0} tools
            </span>
          ) : (
            <span className="chip !py-0 text-[10px] text-faint">not tested</span>
          )}
          {gh.lastError && (
            <span className="truncate text-error" title={gh.lastError}>
              {gh.lastError}
            </span>
          )}
          <button
            className="btn-ghost ml-auto"
            onClick={retest}
            disabled={busy}
          >
            Test
          </button>
        </div>
      )}
      {error && (
        <div className="mt-2 rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
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
  const { data: servers } = useMcpServers();
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
    <section className="card p-4">
      {banner && (
        <div
          className={`mb-3 rounded-[var(--radius)] border px-3 py-2 text-sm ${banner.ok ? "border-success/40 bg-success-soft text-success" : "border-error/40 bg-error-soft text-error"}`}
        >
          {banner.text}
        </div>
      )}
      <div className="mb-3 flex items-start justify-between gap-3">
        <div className="flex items-start gap-2">
          <Boxes size={15} className="mt-0.5 text-faint" />
          <div>
            <h2 className="text-sm font-semibold text-text">MCP servers</h2>
            <p className="mt-0.5 text-xs text-faint">
              Remote Model Context Protocol servers. Sessions pick which to use;
              their tools appear as <code>mcp__&lt;name&gt;__&lt;tool&gt;</code>.
            </p>
          </div>
        </div>
        <button
          className="btn-outline shrink-0 !px-2.5 !py-1.5 text-xs"
          onClick={() => setAdding(true)}
        >
          <Plus size={14} /> Add server
        </button>
      </div>
      <div className="space-y-2.5">
        {generic.length === 0 && !adding && (
          <p className="rounded-[var(--radius)] border border-dashed px-3 py-4 text-center text-sm text-faint">
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
    if (!url.trim()) return setError("URL is required.");
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

  const remove = () => {
    if (isNew) return onDone?.();
    del.mutate(server.name);
  };

  return (
    <RowShell onRemove={remove} removeLabel="Remove MCP server">
      <div className="space-y-3">
        <div className="grid grid-cols-2 gap-3">
          {isNew ? (
            <TextField
              label="Name"
              value={name}
              onChange={(v) => {
                setName(v);
                touch();
              }}
              placeholder="linear"
            />
          ) : (
            <div>
              <RowLabel>Name</RowLabel>
              <div className="truncate py-1.5 font-mono text-sm text-text">
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
              className="input font-mono"
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
              <span className="chip !py-0 text-[10px] text-success">
                enabled · {server.toolCount ?? 0} tools
              </span>
            ) : (
              <span className="chip !py-0 text-[10px] text-faint">not tested</span>
            )}
            {authKind === "OAuth" && connected && (
              <span className="chip !py-0 text-[10px] text-success">
                authorized
              </span>
            )}
            {server.lastError && (
              <span className="truncate text-error" title={server.lastError}>
                {server.lastError}
              </span>
            )}
          </div>
        )}

        {error && (
          <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
            {error}
          </div>
        )}

        <div className="flex justify-end gap-2">
          {!isNew && authKind === "OAuth" && (
            <button
              className="btn-outline"
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
              className="btn-outline"
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
            className="btn-primary"
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
    <section className="card p-4">
      <div className="flex items-center gap-2">
        <Server size={15} className="text-faint" />
        <h2 className="text-sm font-semibold text-text">Server</h2>
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
      <dt className="text-muted">{k}</dt>
      <dd className="truncate font-mono text-text" title={v}>
        {v}
      </dd>
    </>
  );
}
