import {
  Boxes,
  ChevronDown,
  ChevronRight,
  GitBranch,
  Loader2,
  Plus,
  Server,
} from "lucide-react";
import { Fragment, useEffect, useState } from "react";
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
  useMcpServer,
  useMcpServers,
  useTestMcpServer,
  useUpsertMcpServer,
} from "../../hooks/useMcp";
import { useSettings } from "../../hooks/useSettings";
import { askConfirm } from "../../lib/confirm";
import { ReadError } from "../../components/ReadError";
import { RowLabel, RowShell, TextField, SettingsPage } from "./fields";
import { Trans, useTranslation } from "react-i18next";

/** The remote GitHub MCP endpoint reused via the GitHub App connection. */
const GITHUB_MCP_URL = "https://api.githubcopilot.com/mcp/";
/** Row name of the GitHub MCP server (managed from the GitHub section). */
const GITHUB_MCP_NAME = "github";

/**
 * Outbound connections and build info. Every section here saves itself, so the
 * page has no Save button.
 */
export function IntegrationsSettings() {
  const { t } = useTranslation();
  const { data: settings, isLoading, isError } = useSettings();
  return (
    <SettingsPage
        title={t("settingsNav.integrations")}
    >
          {isLoading && (
            <div className="py-16 text-center text-sm text-faint">
              {t("common.loading")}
            </div>
          )}
          {isError && (
            <div className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
              <Trans
                i18nKey="modelsPage.loadFailed"
                components={{ cmd: <code /> }}
              />
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
  const { t } = useTranslation();
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
          <h2 className="section-title">{t("integrations.github")}</h2>
        </div>
      </div>

      <div className="space-y-3">
        {status?.connected ? (
          <div className="flex items-center justify-between rounded-[var(--radius-control)] px-3 py-2 text-sm">
            <span>
              <Trans
                i18nKey="integrations.connectedAs"
                values={{ login: status.login }}
                components={{ login: <span className="font-mono" /> }}
              />
            </span>
            <button
              className="key key-flat text-red-ink"
              onClick={() => disconnect.mutate()}
            >
              {t("integrations.disconnect")}
            </button>
          </div>
        ) : (
          <div className="flex items-center justify-between gap-3 screen px-3 py-2 text-sm text-dim">
            <span>
              {status?.appConfigured ? (
                t("integrations.appConfigured")
              ) : (
                <Trans
                  i18nKey="integrations.noApp"
                  components={{
                    lnk: (
                      <Link
                        to="/admin/github-app"
                        className="text-live-ink underline underline-offset-2"
                      />
                    ),
                  }}
                />
              )}
            </span>
            <a
              className="key shrink-0 aria-disabled:pointer-events-none aria-disabled:opacity-40"
              href={api.github.authUrl()}
              aria-disabled={!status?.appConfigured}
              title={
                status?.appConfigured
                  ? undefined
                  : t("integrations.registerFirst")
              }
              onClick={(e) => {
                if (!status?.appConfigured) e.preventDefault();
              }}
            >
              {t("environment.connectGithub2")}
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
  const { t } = useTranslation();
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
      setError(e instanceof ApiRequestError ? e.message : t("integrations.testFailed"));
    }
  };

  return (
    <div
      className="rounded-[var(--radius-control)] px-3 py-2.5"
      style={{ background: "var(--panel-raised)" }}
    >
      <div className="flex items-center justify-between gap-2">
        <div>
          <p className="text-sm font-medium text-legend">
            {t("integrations.githubTools")}
          </p>
        </div>
        {gh ? (
          <button
            className="key key-flat text-red-ink"
            onClick={() => del.mutate(GITHUB_MCP_NAME)}
          >
            {t("integrations.disable")}
          </button>
        ) : (
          <button className="key" onClick={enable} disabled={busy}>
            {busy ? <Loader2 size={14} className="animate-spin" /> : null}{" "}
            {t("integrations.enable")}
          </button>
        )}
      </div>
      {gh && (
        <div className="mt-2 flex flex-wrap items-center gap-2 text-xs">
          {gh.enabled ? (
            <span className="chip !py-0 text-[0.625rem] text-lamp-ok">
              {t("integrations.enabledTools", { count: gh.toolCount ?? 0 })}
            </span>
          ) : (
            <span className="chip !py-0 text-[0.625rem] text-faint">
              {t("integrations.notTested")}
            </span>
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
            {t("integrations.test")}
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
  const { t } = useTranslation();
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
            <h2 className="section-title">{t("channel.mcpServers2")}</h2>
          </div>
        </div>
        <button
          className="key shrink-0 key-sm"
          onClick={() => setAdding(true)}
        >
          <Plus size={14} /> {t("integrations.addServer")}
        </button>
      </div>
      <div className="space-y-2.5">
        {isError && (
          <ReadError
            what={t("channel.mcpServers")}
            error={loadError}
            testId="mcp-servers-error"
          />
        )}
        {!isError && generic.length === 0 && !adding && (
          <p className="screen px-3 py-4 text-center text-sm text-faint">
{t("integrations.noServers")}
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
  const { t } = useTranslation();
  const upsert = useUpsertMcpServer();
  const del = useDeleteMcpServer();
  const test = useTestMcpServer();
  const connect = useConnectMcpServer();
  const { data: allServers } = useMcpServers();
  const isNew = !server;

  const [name, setName] = useState(server?.name ?? "");
  const [url, setUrl] = useState(server?.url ?? "");
  // The *typed* description, never the discovered one: showing the server's
  // own words in an editable box would turn them into something a person
  // wrote the moment anyone pressed Save.
  const [description, setDescription] = useState(server?.userDescription ?? "");
  const [showTools, setShowTools] = useState(false);
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
        body: {
          name: name.trim(),
          url: url.trim(),
          // "" clears; the server then falls back to whatever the server
          // itself says. Undefined would mean "keep", which is not what an
          // emptied box asks for.
          description: description.trim(),
          auth,
        },
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
              label={t("memoryPage.name")}
              value={name}
              onChange={(v) => {
                setName(v);
                touch();
              }}
              placeholder={t("integrations.namePlaceholder")}
              hint={t("integrations.nameHint")}
            />
          ) : (
            <div>
              <RowLabel>{t("memoryPage.name")}</RowLabel>
              <div className="truncate py-1.5 font-mono text-sm text-legend">
                {name}
              </div>
            </div>
          )}
          <TextField
            label={t("integrations.url")}
            value={url}
            onChange={(v) => {
              setUrl(v);
              touch();
            }}
            placeholder={t("integrations.urlPlaceholder")}
          />
          <TextField
            label={t("integrations.description")}
            value={description}
            onChange={(v) => {
              setDescription(v);
              touch();
            }}
            placeholder={
              server?.description && !server.userDescription
                ? server.description
                : t("integrations.descriptionPlaceholder")
            }
            hint={t("integrations.descriptionHint")}
          />
          <label className="block">
            <RowLabel>{t("integrations.auth")}</RowLabel>
            <select
              className="field font-mono"
              value={authKind}
              onChange={(e) => {
                setAuthKind(e.target.value as "None" | "Bearer" | "OAuth");
                touch();
              }}
            >
              <option value="None">{t("integrations.authNone")}</option>
              <option value="Bearer">{t("integrations.authBearer")}</option>
              <option value="OAuth">{t("integrations.authOAuth")}</option>
            </select>
          </label>
          {authKind === "Bearer" && (
            <TextField
              label={t("integrations.authBearer")}
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
                label={t("integrations.clientId")}
                value={clientId}
                onChange={(v) => {
                  setClientId(v);
                  touch();
                }}
                placeholder={t("integrations.autoRegister")}
              />
              <TextField
                label={t("integrations.clientSecret")}
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
                {t("integrations.enabled")}
              </span>
            ) : (
              <span className="chip !py-0 text-[0.625rem] text-faint">
                {t("integrations.notTested")}
              </span>
            )}
            {/* Shown whenever a catalogue exists, enabled or not: a server
                that is down right now is still worth reading the tools of.
                `undefined` means it has never connected, which is the only
                case with nothing to show. */}
            {server.toolCount !== undefined && (
              <button
                type="button"
                className="chip !py-0 text-[0.625rem] text-legend"
                onClick={() => setShowTools((v) => !v)}
                aria-expanded={showTools}
                data-testid="mcp-tools-toggle"
              >
                {showTools ? (
                  <ChevronDown size={11} />
                ) : (
                  <ChevronRight size={11} />
                )}
                {t("integrations.toolCount", { count: server.toolCount })}
              </button>
            )}
            {authKind === "OAuth" && connected && (
              <span className="chip !py-0 text-[0.625rem] text-lamp-ok">
                {t("integrations.authorized")}
              </span>
            )}
            {server.lastError && (
              <span className="truncate text-red-ink" title={server.lastError}>
                {server.lastError}
              </span>
            )}
          </div>
        )}

        {!isNew && showTools && <McpToolList name={server.name} />}

        {!isNew && server.instructions && (
          <div className="screen px-3 py-2">
            <RowLabel>{t("integrations.serverInstructions")}</RowLabel>
            <p className="mt-1 max-w-prose text-xs leading-relaxed whitespace-pre-wrap text-faint">
              {server.instructions}
            </p>
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
                    e instanceof ApiRequestError
                      ? e.message
                      : t("integrations.connectFailed"),
                  );
                }
              }}
            >
              {connect.isPending ? (
                <Loader2 size={14} className="animate-spin" />
              ) : null}
              {connected ? t("integrations.reauthorize") : t("modelsPage.connect")}
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
              {t("integrations.test")}
            </button>
          )}
          <button
            className="key key-go"
            onClick={save}
            disabled={(!isNew && !dirty) || upsert.isPending}
          >
            {t("common.save")}
          </button>
        </div>
      </div>
    </RowShell>
  );
}

/**
 * The tools one server advertised at its last successful connect, each with
 * its own description.
 *
 * Read from what horsie remembered, not from the server: this list has to load
 * for a server that is currently down, and dialling one from a settings page
 * would make opening a row wait on a network round trip to a third party.
 */
function McpToolList({ name }: { name: string }) {
  const { t } = useTranslation();
  const { data, isPending, isError, error } = useMcpServer(name);

  if (isPending)
    return (
      <div className="screen px-3 py-2 text-xs text-faint">
        <Loader2 size={12} className="mr-1.5 inline animate-spin" />
        {t("common.loading")}
      </div>
    );
  if (isError)
    return (
      <ReadError
        what={t("integrations.tools")}
        error={error}
        testId="mcp-tools-error"
      />
    );

  const tools = data.tools ?? [];
  if (tools.length === 0)
    return (
      <p className="screen px-3 py-2 text-xs text-faint">
        {t("integrations.noTools")}
      </p>
    );

  return (
    <dl
      className="screen grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 px-3 py-2"
      data-testid="mcp-tool-list"
    >
      {tools.map((tool) => (
        <Fragment key={tool.name}>
          <dt className="font-mono text-[0.6875rem] text-legend">
            {tool.name}
          </dt>
          <dd className="min-w-0 text-[0.6875rem] leading-relaxed text-faint">
            {tool.description || t("integrations.noToolDescription")}
          </dd>
        </Fragment>
      ))}
    </dl>
  );
}

function ServerInfoCard({ view }: { view: SettingsView }) {
  const { t } = useTranslation();
  const { info } = view;
  const none = t("integrations.none");
  const rows: [string, string][] = [
    [t("integrations.configFile"), info.configPath || none],
    [t("integrations.database"), info.database || none],
    [t("integrations.stateDir"), info.stateDir],
    [t("integrations.dataDir"), info.dataDir],
    [t("integrations.pluginsDir"), info.pluginsDir],
    [t("integrations.version"), info.version],
  ];
  return (
    <section className="section">
      <div className="flex items-center gap-2">
        <Server size={15} className="text-faint" />
        <h2 className="section-title">{t("integrations.server")}</h2>
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
