import {
  AlertTriangle,
  Boxes,
  Check,
  ChevronRight,
  GitBranch,
  Loader2,
  Plus,
  RotateCcw,
  Save,
  Server,
  Trash2,
} from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useSearchParams } from "react-router-dom";
import { api, ApiRequestError } from "../api/client";
import type {
  McpServerInput,
  McpServerView,
  ModelInput,
  ProviderInput,
  SettingsView,
  VendorInput,
  VendorTestResult,
} from "../api/types";
import { cn } from "../lib/cn";
import {
  useGithubAppConfig,
  useGithubDisconnect,
  useGithubStatus,
  useSaveGithubAppConfig,
} from "../hooks/useGithub";
import {
  useConnectMcpServer,
  useDeleteMcpServer,
  useMcpServers,
  useTestMcpServer,
  useUpsertMcpServer,
} from "../hooks/useMcp";
import { useSettings, useTestVendor, useUpdateSettings } from "../hooks/useSettings";

/** The remote GitHub MCP endpoint reused via the GitHub App connection. */
const GITHUB_MCP_URL = "https://api.githubcopilot.com/mcp/";
/** Row name of the GitHub MCP server (managed from the GitHub section). */
const GITHUB_MCP_NAME = "github";

type ProviderKind = "anthropic" | "openai";

type ProviderDraft = {
  name: string;
  kind: ProviderKind;
  baseUrl: string;
  apiKeyInput: string; // "" = leave the stored key unchanged
  hasInlineKey: boolean;
};

type ModelDraft = {
  alias: string;
  provider: string;
  modelId: string;
  maxTokens: string; // "" = unset
};

type VelosDraft = {
  name: string;
  serverUrl: string;
  image: string;
  advertiseAddress: string;
  tokenInput: string; // "" = keep stored token
  hasInlineToken: boolean;
  runtimeBin: string;
  workspaceRoot: string;
  cpu: string;
  memoryMib: string;
  connectTimeoutSecs: string;
  active: boolean;
  error: string | null;
};

const toProviderDrafts = (v: SettingsView): ProviderDraft[] =>
  v.providers.map((p) => ({
    name: p.name,
    kind: p.kind === "openai" ? "openai" : "anthropic",
    baseUrl: p.baseUrl ?? "",
    apiKeyInput: "",
    hasInlineKey: p.hasInlineKey,
  }));

const toModelDrafts = (v: SettingsView): ModelDraft[] =>
  v.models.map((m) => ({
    alias: m.alias,
    provider: m.provider,
    modelId: m.modelId,
    maxTokens: m.maxTokens != null ? String(m.maxTokens) : "",
  }));

const num = (n: number | undefined): string => (n != null ? String(n) : "");

// Velos drafts come from the generic vendor list: the vendors whose config is
// the `Velos` variant. Their name lives on the vendor, the fields on the config.
const toVelosDrafts = (v: SettingsView): VelosDraft[] =>
  v.vendors.flatMap((vd) =>
    vd.config?.kind === "Velos"
      ? [
          {
            name: vd.name,
            serverUrl: vd.config.value.serverUrl,
            image: vd.config.value.image,
            advertiseAddress: vd.config.value.advertiseAddress,
            tokenInput: "",
            hasInlineToken: vd.config.value.hasInlineToken,
            runtimeBin: vd.config.value.runtimeBin,
            workspaceRoot: vd.config.value.workspaceRoot,
            cpu: num(vd.config.value.cpu),
            memoryMib: num(vd.config.value.memoryMib),
            connectTimeoutSecs: num(vd.config.value.connectTimeoutSecs),
            active: vd.active,
            error: vd.error ?? null,
          },
        ]
      : [],
  );

export function SettingsPage() {
  const { data: settings, isLoading, isError } = useSettings();
  const update = useUpdateSettings();
  const testVendor = useTestVendor();

  const [providers, setProviders] = useState<ProviderDraft[]>([]);
  const [models, setModels] = useState<ModelDraft[]>([]);
  const [velos, setVelos] = useState<VelosDraft[]>([]);
  const [defaultVendor, setDefaultVendor] = useState("");
  const [dirty, setDirty] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
  const [velosTests, setVelosTests] = useState<
    Record<string, { pending: boolean; result: VendorTestResult | null }>
  >({});

  const runVelosTest = async (name: string) => {
    setVelosTests((m) => ({
      ...m,
      [name]: { pending: true, result: m[name]?.result ?? null },
    }));
    try {
      const result = await testVendor.mutateAsync(name);
      setVelosTests((m) => ({ ...m, [name]: { pending: false, result } }));
    } catch (e) {
      setVelosTests((m) => ({
        ...m,
        [name]: {
          pending: false,
          result: {
            ok: false,
            identity: undefined,
            error: e instanceof ApiRequestError ? e.message : "Test failed.",
          },
        },
      }));
    }
  };

  // (Re)seed the form from the server view on load and after a successful save.
  useEffect(() => {
    if (!settings) return;
    setProviders(toProviderDrafts(settings));
    setModels(toModelDrafts(settings));
    setVelos(toVelosDrafts(settings));
    setDefaultVendor(settings.defaultVendor);
    setDirty(false);
    setLocalError(null);
  }, [settings]);

  const providerNames = useMemo(
    () => providers.map((p) => p.name.trim()).filter(Boolean),
    [providers],
  );

  const touch = () => setDirty(true);

  const save = () => {
    setLocalError(null);
    const uniq = (xs: string[]) => new Set(xs).size === xs.length;
    if (providers.some((p) => !p.name.trim()))
      return setLocalError("Every provider needs a name.");
    if (!uniq(providers.map((p) => p.name.trim())))
      return setLocalError("Provider names must be unique.");
    if (models.some((m) => !m.alias.trim()))
      return setLocalError("Every model needs an alias.");
    if (!uniq(models.map((m) => m.alias.trim())))
      return setLocalError("Model aliases must be unique.");
    for (const m of models)
      if (m.maxTokens.trim() && !/^\d+$/.test(m.maxTokens.trim()))
        return setLocalError(`Max tokens for "${m.alias}" must be a number.`);
    if (velos.some((v) => !v.name.trim()))
      return setLocalError("Every velos vendor needs a name.");
    if (!uniq(velos.map((v) => v.name.trim())))
      return setLocalError("Velos vendor names must be unique.");
    for (const v of velos) {
      if (!v.serverUrl.trim() || !v.image.trim() || !v.advertiseAddress.trim())
        return setLocalError(
          `Velos vendor "${v.name}" needs a server URL, image, and advertise address.`,
        );
      if (v.advertiseAddress.trim() && !/^[^:]+:\d+$/.test(v.advertiseAddress.trim()))
        return setLocalError(
          `Advertise address for "${v.name}" must be host:port.`,
        );
      for (const [label, val] of [
        ["CPU", v.cpu],
        ["memory", v.memoryMib],
        ["connect timeout", v.connectTimeoutSecs],
      ] as const)
        if (val.trim() && !/^\d+$/.test(val.trim()))
          return setLocalError(`${label} for "${v.name}" must be a number.`);
    }

    const providerInputs: ProviderInput[] = providers.map((p) => ({
      name: p.name.trim(),
      kind: p.kind,
      baseUrl: p.baseUrl.trim() || undefined,
      apiKey: p.apiKeyInput === "" ? undefined : p.apiKeyInput,
    }));
    const modelInputs: ModelInput[] = models.map((m) => ({
      alias: m.alias.trim(),
      provider: m.provider,
      modelId: m.modelId.trim(),
      maxTokens: m.maxTokens.trim() ? Number(m.maxTokens.trim()) : undefined,
    }));
    const vendorInputs: VendorInput[] = velos.map((v) => ({
      name: v.name.trim(),
      config: {
        kind: "Velos",
        value: {
          serverUrl: v.serverUrl.trim(),
          image: v.image.trim(),
          advertiseAddress: v.advertiseAddress.trim(),
          token: v.tokenInput === "" ? undefined : v.tokenInput,
          runtimeBin: v.runtimeBin.trim() || undefined,
          workspaceRoot: v.workspaceRoot.trim() || undefined,
          cpu: v.cpu.trim() ? Number(v.cpu.trim()) : undefined,
          memoryMib: v.memoryMib.trim() ? Number(v.memoryMib.trim()) : undefined,
          connectTimeoutSecs: v.connectTimeoutSecs.trim()
            ? Number(v.connectTimeoutSecs.trim())
            : undefined,
        },
      },
    }));

    update.mutate(
      {
        providers: providerInputs,
        models: modelInputs,
        vendors: vendorInputs,
        defaultVendor: defaultVendor || undefined,
      },
      {
        onSuccess: (view) => {
          for (const vd of view.vendors) {
            if (vd.config?.kind === "Velos") runVelosTest(vd.name);
          }
        },
      },
    );
  };

  const discard = () => {
    if (!settings) return;
    setProviders(toProviderDrafts(settings));
    setModels(toModelDrafts(settings));
    setVelos(toVelosDrafts(settings));
    setDefaultVendor(settings.defaultVendor);
    setDirty(false);
    setLocalError(null);
    update.reset();
  };

  const saveError =
    update.error instanceof ApiRequestError
      ? update.error.message
      : update.isError
        ? "Failed to save settings."
        : null;

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <header className="flex items-center gap-3 border-b px-6 py-3.5">
        <div>
          <h1 className="text-[15px] font-semibold text-text">Settings</h1>
          <p className="text-xs text-faint">
            Models, providers, and runtime vendors — stored in the server database.
          </p>
        </div>
        <div className="ml-auto flex items-center gap-2">
          {dirty && !update.isPending && (
            <span className="text-xs text-faint">Unsaved changes</span>
          )}
          {update.isSuccess && !dirty && (
            <span className="flex items-center gap-1 text-xs text-success">
              <Check size={13} /> Saved
            </span>
          )}
          <button className="btn-ghost" onClick={discard} disabled={!dirty}>
            <RotateCcw size={14} /> Discard
          </button>
          <button
            className="btn-primary"
            onClick={save}
            disabled={!dirty || update.isPending}
          >
            {update.isPending ? (
              <Loader2 size={15} className="animate-spin" />
            ) : (
              <Save size={15} />
            )}
            Save changes
          </button>
        </div>
      </header>

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

          {(localError || saveError) && (
            <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
              {localError ?? saveError}
            </div>
          )}
          {settings?.restartRequired && (
            <div className="flex items-start gap-2 rounded-[var(--radius)] border border-warning/40 bg-warning-soft px-3 py-2 text-sm text-warning">
              <AlertTriangle size={15} className="mt-0.5 shrink-0" />
              A vendor's server URL, listen address, or advertise host changed
              and needs a server restart to take effect. Other vendor edits
              apply immediately.
            </div>
          )}

          {settings && (
            <>
              <Section
                title="Providers"
                desc="Anthropic-compatible API endpoints."
                onAdd={() => {
                  setProviders((ps) => [
                    ...ps,
                    {
                      name: "",
                      kind: "anthropic",
                      baseUrl: "",
                      apiKeyInput: "",
                      hasInlineKey: false,
                    },
                  ]);
                  touch();
                }}
                addLabel="Add provider"
                empty={providers.length === 0 ? "No providers yet." : null}
              >
                {providers.map((p, i) => (
                  <ProviderRow
                    key={i}
                    draft={p}
                    onChange={(next) => {
                      setProviders((ps) => ps.map((x, j) => (j === i ? next : x)));
                      touch();
                    }}
                    onRemove={() => {
                      setProviders((ps) => ps.filter((_, j) => j !== i));
                      touch();
                    }}
                  />
                ))}
              </Section>

              <Section
                title="Models"
                desc="Aliases sessions pick from. Each routes to a provider's model id."
                onAdd={() => {
                  setModels((ms) => [
                    ...ms,
                    { alias: "", provider: providerNames[0] ?? "", modelId: "", maxTokens: "" },
                  ]);
                  touch();
                }}
                addLabel="Add model"
                empty={models.length === 0 ? "No models yet." : null}
              >
                {models.map((m, i) => (
                  <ModelRow
                    key={i}
                    draft={m}
                    providerNames={providerNames}
                    onChange={(next) => {
                      setModels((ms) => ms.map((x, j) => (j === i ? next : x)));
                      touch();
                    }}
                    onRemove={() => {
                      setModels((ms) => ms.filter((_, j) => j !== i));
                      touch();
                    }}
                  />
                ))}
              </Section>

              <VendorsCard
                view={settings}
                defaultVendor={defaultVendor}
                onChange={(v) => {
                  setDefaultVendor(v);
                  touch();
                }}
              />

              <Section
                title="Velos remote runtimes"
                desc="Remote container runtimes (velos clusters). Add as many as you need — all changes apply immediately."
                onAdd={() => {
                  setVelos((vs) => [
                    ...vs,
                    {
                      name: "",
                      serverUrl: "",
                      image: "",
                      advertiseAddress: "",
                      tokenInput: "",
                      hasInlineToken: false,
                      runtimeBin: "",
                      workspaceRoot: "",
                      cpu: "",
                      memoryMib: "",
                      connectTimeoutSecs: "",
                      active: false,
                      error: null,
                    },
                  ]);
                  touch();
                }}
                addLabel="Add velos vendor"
                empty={velos.length === 0 ? "No velos vendors — sessions run locally." : null}
              >
                {velos.map((v, i) => (
                  <VelosRow
                    key={i}
                    draft={v}
                    onChange={(next) => {
                      setVelos((vs) => vs.map((x, j) => (j === i ? next : x)));
                      touch();
                    }}
                    onRemove={() => {
                      setVelos((vs) => vs.filter((_, j) => j !== i));
                      touch();
                    }}
                    testDisabled={dirty}
                    test={velosTests[v.name]}
                    onTest={() => runVelosTest(v.name)}
                  />
                ))}
              </Section>

              <GithubSection />

              <McpSection />

              <ServerInfoCard view={settings} />
            </>
          )}
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

function Section({
  title,
  desc,
  children,
  onAdd,
  addLabel,
  empty,
}: {
  title: string;
  desc: string;
  children: ReactNode;
  onAdd: () => void;
  addLabel: string;
  empty: string | null;
}) {
  return (
    <section className="card p-4">
      <div className="mb-3 flex items-start justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold text-text">{title}</h2>
          <p className="mt-0.5 text-xs text-faint">{desc}</p>
        </div>
        <button className="btn-outline shrink-0 !px-2.5 !py-1.5 text-xs" onClick={onAdd}>
          <Plus size={14} /> {addLabel}
        </button>
      </div>
      <div className="space-y-2.5">
        {empty && (
          <p className="rounded-[var(--radius)] border border-dashed px-3 py-4 text-center text-sm text-faint">
            {empty}
          </p>
        )}
        {children}
      </div>
    </section>
  );
}

function RowLabel({ children }: { children: ReactNode }) {
  return (
    <span className="mb-1 block text-[11px] font-semibold text-muted">
      {children}
    </span>
  );
}

function TextField({
  label,
  value,
  onChange,
  placeholder,
  type,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  type?: string;
}) {
  return (
    <label className="block">
      <RowLabel>{label}</RowLabel>
      <input
        className="input font-mono"
        type={type}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
      />
    </label>
  );
}

function RowShell({
  onRemove,
  removeLabel,
  children,
}: {
  onRemove: () => void;
  removeLabel: string;
  children: ReactNode;
}) {
  return (
    <div
      className="rounded-[var(--radius)] border p-3"
      style={{ background: "var(--surface-2)" }}
    >
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">{children}</div>
        <button
          className="btn-icon shrink-0 text-faint hover:text-error"
          onClick={onRemove}
          aria-label={removeLabel}
        >
          <Trash2 size={15} />
        </button>
      </div>
    </div>
  );
}

function ProviderRow({
  draft,
  onChange,
  onRemove,
}: {
  draft: ProviderDraft;
  onChange: (next: ProviderDraft) => void;
  onRemove: () => void;
}) {
  const set = (patch: Partial<ProviderDraft>) => onChange({ ...draft, ...patch });
  return (
    <RowShell onRemove={onRemove} removeLabel="Remove provider">
      <div className="grid grid-cols-2 gap-3">
        <TextField label="Name" value={draft.name} onChange={(v) => set({ name: v })} placeholder="anthropic" />
        <label className="block">
          <RowLabel>Kind</RowLabel>
          <select
            className="input font-mono"
            value={draft.kind}
            onChange={(e) => set({ kind: e.target.value as ProviderKind })}
          >
            <option value="anthropic">Anthropic</option>
            <option value="openai">OpenAI-compatible</option>
          </select>
        </label>
        <TextField
          label="Base URL (optional)"
          value={draft.baseUrl}
          onChange={(v) => set({ baseUrl: v })}
          placeholder={
            draft.kind === "openai" ? "http://127.0.0.1:11434" : "https://api.anthropic.com"
          }
        />
        <TextField
          label="Inline key"
          type="password"
          value={draft.apiKeyInput}
          onChange={(v) => set({ apiKeyInput: v })}
          placeholder={draft.hasInlineKey ? "•••• stored — blank keeps it" : "not set"}
        />
      </div>
    </RowShell>
  );
}

function ModelRow({
  draft,
  providerNames,
  onChange,
  onRemove,
}: {
  draft: ModelDraft;
  providerNames: string[];
  onChange: (next: ModelDraft) => void;
  onRemove: () => void;
}) {
  const set = (patch: Partial<ModelDraft>) => onChange({ ...draft, ...patch });
  const options =
    draft.provider && !providerNames.includes(draft.provider)
      ? [draft.provider, ...providerNames]
      : providerNames;
  return (
    <RowShell onRemove={onRemove} removeLabel="Remove model">
      <div className="grid grid-cols-2 gap-3">
        <TextField label="Alias" value={draft.alias} onChange={(v) => set({ alias: v })} placeholder="sonnet" />
        <label className="block">
          <RowLabel>Provider</RowLabel>
          <select
            className="input font-mono"
            value={draft.provider}
            onChange={(e) => set({ provider: e.target.value })}
          >
            {options.length === 0 && <option value="">—</option>}
            {options.map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </label>
        <TextField
          label="Model id"
          value={draft.modelId}
          onChange={(v) => set({ modelId: v })}
          placeholder="claude-sonnet-4-6"
        />
        <TextField
          label="Max tokens (optional)"
          value={draft.maxTokens}
          onChange={(v) => set({ maxTokens: v })}
          placeholder="8192"
        />
      </div>
    </RowShell>
  );
}

function VelosRow({
  draft,
  onChange,
  onRemove,
  testDisabled,
  test,
  onTest,
}: {
  draft: VelosDraft;
  onChange: (next: VelosDraft) => void;
  onRemove: () => void;
  testDisabled: boolean;
  test: { pending: boolean; result: VendorTestResult | null } | undefined;
  onTest: () => void;
}) {
  const [advanced, setAdvanced] = useState(false);
  const set = (patch: Partial<VelosDraft>) => onChange({ ...draft, ...patch });
  return (
    <RowShell onRemove={onRemove} removeLabel="Remove velos vendor">
      <div className="space-y-3">
        <div className="grid grid-cols-2 gap-3">
          <TextField label="Name" value={draft.name} onChange={(v) => set({ name: v })} placeholder="cluster-a" />
          <TextField
            label="Server URL"
            value={draft.serverUrl}
            onChange={(v) => set({ serverUrl: v })}
            placeholder="http://velos.internal:8080"
          />
          <TextField
            label="Runtime image"
            value={draft.image}
            onChange={(v) => set({ image: v })}
            placeholder="ghcr.io/…/horsie-runtime:tag"
          />
          <TextField
            label="Advertise address"
            value={draft.advertiseAddress}
            onChange={(v) => set({ advertiseAddress: v })}
            placeholder="10.0.0.5:3789"
          />
          <TextField
            label="Inline token"
            type="password"
            value={draft.tokenInput}
            onChange={(v) => set({ tokenInput: v })}
            placeholder={draft.hasInlineToken ? "•••• stored — blank keeps it" : "not set"}
          />
        </div>

        <button
          type="button"
          className="flex items-center gap-1 text-xs font-medium text-muted transition-colors hover:text-text"
          onClick={() => setAdvanced((a) => !a)}
        >
          <ChevronRight size={13} className={cn("transition-transform", advanced && "rotate-90")} />
          Advanced
        </button>
        {advanced && (
          <div className="grid grid-cols-2 gap-3 border-t pt-3">
            <TextField
              label="Runtime bin"
              value={draft.runtimeBin}
              onChange={(v) => set({ runtimeBin: v })}
              placeholder="horsie-runtime"
            />
            <TextField
              label="Workspace root"
              value={draft.workspaceRoot}
              onChange={(v) => set({ workspaceRoot: v })}
              placeholder="/workspace"
            />
            <TextField label="CPU" value={draft.cpu} onChange={(v) => set({ cpu: v })} placeholder="2" />
            <TextField
              label="Memory (MiB)"
              value={draft.memoryMib}
              onChange={(v) => set({ memoryMib: v })}
              placeholder="1024"
            />
            <TextField
              label="Connect timeout (s)"
              value={draft.connectTimeoutSecs}
              onChange={(v) => set({ connectTimeoutSecs: v })}
              placeholder="60"
            />
          </div>
        )}
        <div className="flex items-center gap-2">
          <button
            type="button"
            className="btn-outline text-xs"
            disabled={testDisabled || test?.pending}
            title={testDisabled ? "Save changes to test" : undefined}
            onClick={onTest}
          >
            {test?.pending && <Loader2 size={13} className="animate-spin" />}
            Test connection
          </button>
          {test?.result &&
            (test.result.ok ? (
              <span className="chip !py-0 text-[10px] text-success">
                Connected as {test.result.identity}
              </span>
            ) : (
              <span
                className="truncate text-[11px] text-error"
                title={test.result.error ?? undefined}
              >
                {test.result.error}
              </span>
            ))}
        </div>
        {draft.error && <p className="text-[11px] text-error">{draft.error}</p>}
        {!draft.active && !draft.error && draft.name.trim() && (
          <p className="text-[11px] text-faint">Not loaded yet.</p>
        )}
      </div>
    </RowShell>
  );
}

function VendorsCard({
  view,
  defaultVendor,
  onChange,
}: {
  view: SettingsView;
  defaultVendor: string;
  onChange: (v: string) => void;
}) {
  return (
    <section className="card p-4">
      <h2 className="text-sm font-semibold text-text">Default vendor</h2>
      <p className="mt-0.5 text-xs text-faint">
        Where new sessions run unless they pick another. Only loaded vendors can
        be the default.
      </p>
      <div className="mt-3 space-y-1.5">
        {view.vendors.map((v) => (
          <label
            key={v.name}
            className="flex items-center gap-2.5 rounded-[var(--radius)] border px-3 py-2 text-sm"
            style={{ background: "var(--surface-2)" }}
          >
            <input
              type="radio"
              name="default-vendor"
              className="accent-[var(--accent)]"
              checked={defaultVendor === v.name}
              disabled={!v.active}
              onChange={() => onChange(v.name)}
            />
            <span className="font-mono text-text">{v.name}</span>
            {!v.active && <span className="chip !py-0 text-[10px]">not loaded</span>}
            {defaultVendor === v.name && (
              <span className="ml-auto text-xs text-faint">default</span>
            )}
          </label>
        ))}
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
