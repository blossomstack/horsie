import { useEffect, useMemo, useState } from "react";
import { ApiRequestError } from "../../api/client";
import type {
  ModelCard,
  ModelInput,
  ProviderInput,
  SettingsView,
} from "../../api/types";
import { useModelCardSearch } from "../../hooks/useModelCards";
import { useSettings, useUpdateSettings } from "../../hooks/useSettings";
import { RowLabel, RowShell, Section, TextField } from "./fields";
import { SettingsHeader } from "./SettingsHeader";
import { usePublishDirty } from "./dirty";

type ProviderKind = "anthropic" | "openai";

type ProviderDraft = {
  name: string;
  kind: ProviderKind;
  baseUrl: string;
  apiKeyInput: string; // "" = leave the stored key unchanged
  hasInlineKey: boolean;
  keepThinkingSignature: boolean;
};

type ModelDraft = {
  alias: string;
  provider: string;
  modelId: string;
  maxTokens: string; // "" = unset
  contextWindow: string; // "" = unset (server applies a built-in default)
};

const toProviderDrafts = (v: SettingsView): ProviderDraft[] =>
  v.providers.map((p) => ({
    name: p.name,
    kind: p.kind === "openai" ? "openai" : "anthropic",
    baseUrl: p.baseUrl ?? "",
    apiKeyInput: "",
    hasInlineKey: p.hasInlineKey,
    keepThinkingSignature: p.keepThinkingSignature,
  }));

const toModelDrafts = (v: SettingsView): ModelDraft[] =>
  v.models.map((m) => ({
    alias: m.alias,
    provider: m.provider,
    modelId: m.modelId,
    maxTokens: m.maxTokens != null ? String(m.maxTokens) : "",
    contextWindow: m.contextWindow != null ? String(m.contextWindow) : "",
  }));

/**
 * Providers and the model aliases that route to them. Saves only its own two
 * collections — `SettingsUpdate` replaces just the fields it carries, so the
 * Runtimes page's vendors are untouched by a save here.
 */
export function ModelsSettings() {
  const { data: settings, isLoading, isError } = useSettings();
  const update = useUpdateSettings();

  const [providers, setProviders] = useState<ProviderDraft[]>([]);
  const [models, setModels] = useState<ModelDraft[]>([]);
  const [dirty, setDirty] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
  usePublishDirty(dirty);

  // (Re)seed the form from the server view on load and after a successful save.
  useEffect(() => {
    if (!settings) return;
    setProviders(toProviderDrafts(settings));
    setModels(toModelDrafts(settings));
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
    for (const m of models)
      if (m.contextWindow.trim() && !/^\d+$/.test(m.contextWindow.trim()))
        return setLocalError(`Context window for "${m.alias}" must be a number.`);

    const providerInputs: ProviderInput[] = providers.map((p) => ({
      name: p.name.trim(),
      kind: p.kind,
      baseUrl: p.baseUrl.trim() || undefined,
      apiKey: p.apiKeyInput === "" ? undefined : p.apiKeyInput,
      keepThinkingSignature: p.keepThinkingSignature,
    }));
    const modelInputs: ModelInput[] = models.map((m) => ({
      alias: m.alias.trim(),
      provider: m.provider,
      modelId: m.modelId.trim(),
      maxTokens: m.maxTokens.trim() ? Number(m.maxTokens.trim()) : undefined,
      contextWindow: m.contextWindow.trim()
        ? Number(m.contextWindow.trim())
        : undefined,
    }));

    update.mutate({ providers: providerInputs, models: modelInputs });
  };

  const discard = () => {
    if (!settings) return;
    setProviders(toProviderDrafts(settings));
    setModels(toModelDrafts(settings));
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
      <SettingsHeader
        title="Models & providers"
        desc="API endpoints and the model aliases sessions pick from."
        dirty={dirty}
        saved={update.isSuccess}
        saving={update.isPending}
        onSave={save}
        onDiscard={discard}
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

          {(localError || saveError) && (
            <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
              {localError ?? saveError}
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
                      keepThinkingSignature: false,
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
                    {
                      alias: "",
                      provider: providerNames[0] ?? "",
                      modelId: "",
                      maxTokens: "",
                      contextWindow: "",
                    },
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
            </>
          )}
        </div>
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
        {draft.kind === "anthropic" && (
          <label className="col-span-2 flex items-start gap-2 text-sm">
            <input
              type="checkbox"
              className="mt-1"
              checked={draft.keepThinkingSignature}
              onChange={(e) => set({ keepThinkingSignature: e.target.checked })}
            />
            <span>
              Keep thinking signatures
              <span className="block text-xs opacity-70">
                Required for api.anthropic.com, which validates them on replay. Leave off for
                Anthropic-compatible endpoints — the blobs are several KB per thinking block and
                nothing reads them.
              </span>
            </span>
          </label>
        )}
      </div>
    </RowShell>
  );
}

/** The model-id input with card-backed autocomplete: typing queries the
 * catalog by prefix; picking a suggestion sets the id and prefills the
 * limit fields that are still empty. Prefill is a one-time copy — every
 * field stays editable, and no link to the card is kept. */
function ModelIdField({
  draft,
  set,
}: {
  draft: ModelDraft;
  set: (patch: Partial<ModelDraft>) => void;
}) {
  const [focused, setFocused] = useState(false);
  const [debounced, setDebounced] = useState(draft.modelId);
  useEffect(() => {
    const t = setTimeout(() => setDebounced(draft.modelId), 200);
    return () => clearTimeout(t);
  }, [draft.modelId]);
  const query = debounced.trim();
  const { data: suggestions } = useModelCardSearch(query, focused && query.length > 0);
  const show = focused && query.length > 0 && (suggestions?.length ?? 0) > 0;

  const pick = (card: ModelCard) => {
    set({
      modelId: card.modelId,
      maxTokens:
        draft.maxTokens === "" && card.maxTokens != null
          ? String(card.maxTokens)
          : draft.maxTokens,
      contextWindow:
        draft.contextWindow === "" && card.contextWindow != null
          ? String(card.contextWindow)
          : draft.contextWindow,
    });
    setFocused(false);
  };

  return (
    <label className="relative block">
      <RowLabel>Model id</RowLabel>
      <input
        className="input font-mono"
        value={draft.modelId}
        onChange={(e) => set({ modelId: e.target.value })}
        onFocus={() => setFocused(true)}
        // Delay so an onMouseDown on a suggestion fires before the list hides.
        onBlur={() => setTimeout(() => setFocused(false), 150)}
        placeholder="claude-sonnet-4-6"
        data-testid="model-id-input"
      />
      {show && (
        <ul
          className="absolute z-10 mt-1 max-h-48 w-full overflow-y-auto rounded-[var(--radius)] border shadow-lg"
          style={{ background: "var(--surface)" }}
          data-testid="model-card-suggestions"
        >
          {suggestions!.map((c) => (
            <li key={c.modelId}>
              <button
                type="button"
                className="flex w-full items-baseline justify-between gap-2 px-2.5 py-1.5 text-left text-xs hover:bg-surface-2"
                onMouseDown={(e) => {
                  e.preventDefault();
                  pick(c);
                }}
                data-testid={`model-card-suggestion-${c.modelId}`}
              >
                <span className="font-mono text-text">{c.modelId}</span>
                <span className="truncate text-faint">
                  {c.name}
                  {c.contextWindow != null
                    ? ` · ${c.contextWindow.toLocaleString()} ctx`
                    : ""}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </label>
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
        <ModelIdField draft={draft} set={set} />
        <TextField
          label="Max tokens (optional)"
          value={draft.maxTokens}
          onChange={(v) => set({ maxTokens: v })}
          placeholder="8192"
        />
        <TextField
          label="Context window (optional)"
          value={draft.contextWindow}
          onChange={(v) => set({ contextWindow: v })}
          placeholder="200000"
        />
      </div>
    </RowShell>
  );
}
