import {
  AlertTriangle,
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
  ModelInput,
  ProviderInput,
  SettingsView,
  VendorInput,
} from "../api/types";
import { cn } from "../lib/cn";
import {
  useGithubAppConfig,
  useGithubDisconnect,
  useGithubStatus,
  useSaveGithubAppConfig,
} from "../hooks/useGithub";
import { useSettings, useUpdateSettings } from "../hooks/useSettings";

type ProviderDraft = {
  name: string;
  baseUrl: string;
  apiKeyEnv: string;
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
  advertiseHost: string;
  tokenInput: string; // "" = keep stored token
  hasInlineToken: boolean;
  tokenEnv: string;
  runtimeBin: string;
  workspaceRoot: string;
  listen: string;
  cpu: string;
  memoryMib: string;
  connectTimeoutSecs: string;
  active: boolean;
};

const toProviderDrafts = (v: SettingsView): ProviderDraft[] =>
  v.providers.map((p) => ({
    name: p.name,
    baseUrl: p.baseUrl ?? "",
    apiKeyEnv: p.apiKeyEnv ?? "",
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
            advertiseHost: vd.config.value.advertiseHost,
            tokenInput: "",
            hasInlineToken: vd.config.value.hasInlineToken,
            tokenEnv: vd.config.value.tokenEnv ?? "",
            runtimeBin: vd.config.value.runtimeBin,
            workspaceRoot: vd.config.value.workspaceRoot,
            listen: vd.config.value.listen,
            cpu: num(vd.config.value.cpu),
            memoryMib: num(vd.config.value.memoryMib),
            connectTimeoutSecs: num(vd.config.value.connectTimeoutSecs),
            active: vd.active,
          },
        ]
      : [],
  );

export function SettingsPage() {
  const { data: settings, isLoading, isError } = useSettings();
  const update = useUpdateSettings();

  const [providers, setProviders] = useState<ProviderDraft[]>([]);
  const [models, setModels] = useState<ModelDraft[]>([]);
  const [velos, setVelos] = useState<VelosDraft[]>([]);
  const [defaultVendor, setDefaultVendor] = useState("");
  const [dirty, setDirty] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);

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
      if (!v.serverUrl.trim() || !v.image.trim() || !v.advertiseHost.trim())
        return setLocalError(
          `Velos vendor "${v.name}" needs a server URL, image, and advertise host.`,
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
      kind: "anthropic",
      baseUrl: p.baseUrl.trim() || undefined,
      apiKeyEnv: p.apiKeyEnv.trim() || undefined,
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
          advertiseHost: v.advertiseHost.trim(),
          token: v.tokenInput === "" ? undefined : v.tokenInput,
          tokenEnv: v.tokenEnv.trim() || undefined,
          runtimeBin: v.runtimeBin.trim() || undefined,
          workspaceRoot: v.workspaceRoot.trim() || undefined,
          listen: v.listen.trim() || undefined,
          cpu: v.cpu.trim() ? Number(v.cpu.trim()) : undefined,
          memoryMib: v.memoryMib.trim() ? Number(v.memoryMib.trim()) : undefined,
          connectTimeoutSecs: v.connectTimeoutSecs.trim()
            ? Number(v.connectTimeoutSecs.trim())
            : undefined,
        },
      },
    }));

    update.mutate({
      providers: providerInputs,
      models: modelInputs,
      vendors: vendorInputs,
      defaultVendor: defaultVendor || undefined,
    });
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
              Velos vendor changes are saved but need a server restart to become
              active.
            </div>
          )}

          {settings && (
            <>
              <Section
                title="Providers"
                desc="Anthropic-compatible API endpoints. Prefer an env var for the key to keep secrets out of the database."
                onAdd={() => {
                  setProviders((ps) => [
                    ...ps,
                    { name: "", baseUrl: "", apiKeyEnv: "", apiKeyInput: "", hasInlineKey: false },
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
                desc="Remote container runtimes (velos clusters). Add as many as you need; changes apply on the next server restart."
                onAdd={() => {
                  setVelos((vs) => [
                    ...vs,
                    {
                      name: "",
                      serverUrl: "",
                      image: "",
                      advertiseHost: "",
                      tokenInput: "",
                      hasInlineToken: false,
                      tokenEnv: "",
                      runtimeBin: "",
                      workspaceRoot: "",
                      listen: "",
                      cpu: "",
                      memoryMib: "",
                      connectTimeoutSecs: "",
                      active: false,
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
                  />
                ))}
              </Section>

              <GithubSection />

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
              className="btn-outline"
              href={api.github.authUrl()}
              aria-disabled={!status?.appConfigured}
              onClick={(e) => {
                if (!status?.appConfigured) e.preventDefault();
              }}
            >
              Connect GitHub
            </a>
          </div>
        )}

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
        <TextField
          label="Base URL (optional)"
          value={draft.baseUrl}
          onChange={(v) => set({ baseUrl: v })}
          placeholder="https://api.anthropic.com"
        />
        <TextField
          label="API key env var"
          value={draft.apiKeyEnv}
          onChange={(v) => set({ apiKeyEnv: v })}
          placeholder="ANTHROPIC_API_KEY"
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
}: {
  draft: VelosDraft;
  onChange: (next: VelosDraft) => void;
  onRemove: () => void;
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
            label="Advertise host"
            value={draft.advertiseHost}
            onChange={(v) => set({ advertiseHost: v })}
            placeholder="10.0.0.5"
          />
          <TextField
            label="Token env var"
            value={draft.tokenEnv}
            onChange={(v) => set({ tokenEnv: v })}
            placeholder="VELOS_TOKEN"
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
            <TextField label="Listen" value={draft.listen} onChange={(v) => set({ listen: v })} placeholder="0.0.0.0:0" />
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
        {!draft.active && draft.name.trim() && (
          <p className="text-[11px] text-faint">Not loaded — restart to activate.</p>
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
