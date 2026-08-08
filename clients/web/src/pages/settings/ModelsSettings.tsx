import { Pencil, Plug, Trash2, X } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { ApiRequestError } from "../../api/client";
import type {
  ModelCard,
  ModelInput,
  ModelView,
  ProviderInput,
  ProviderView,
} from "../../api/types";
import { ChatGptSignIn } from "./ChatGptSignIn";
import { useModelCardSearch } from "../../hooks/useModelCards";
import {
  useRefreshSettings,
  useSettings,
  useUpdateSettings,
} from "../../hooks/useSettings";
import {
  ListRow,
  RowAction,
  RowLabel,
  Section,
  SettingsPane,
  TextField,
} from "./fields";
import { SettingsHeader } from "./SettingsHeader";

type ProviderKind = "anthropic" | "openai" | "openai-responses" | "chatgpt";

const PROVIDER_KINDS: ProviderKind[] = [
  "anthropic",
  "openai",
  "openai-responses",
  "chatgpt",
];

const KIND_LABELS: Record<ProviderKind, string> = {
  anthropic: "Anthropic",
  openai: "OpenAI-compatible",
  "openai-responses": "OpenAI Responses",
  chatgpt: "ChatGPT plan",
};

const KIND_PLACEHOLDERS: Record<ProviderKind, string> = {
  anthropic: "https://api.anthropic.com",
  openai: "http://127.0.0.1:11434",
  // Bare hosts: the client appends the protocol's own path.
  "openai-responses": "https://api.openai.com",
  chatgpt: "https://chatgpt.com",
};

/** A ChatGPT provider is authorised by signing in, not by a stored key. */
const usesApiKey = (kind: ProviderKind) => kind !== "chatgpt";

const EFFORTS = ["none", "minimal", "low", "medium", "high", "xhigh", "max"] as const;
const DIALECTS = [
  "",
  "anthropic_effort",
  "anthropic_always_on",
  "anthropic_budget",
  "openai_effort",
  "zai_thinking",
  "kimi_thinking",
  "none",
] as const;

/** What a provider's credential lamp says, given its kind. A ChatGPT plan is
 * authorized by signing in, so "No key" would be both wrong and unactionable. */
const credentialWords = (kind: string, has: boolean): string =>
  kind === "chatgpt" ? (has ? "Connected" : "Not connected") : has ? "Key set" : "No key";

const credentialHint = (kind: string, has: boolean): string =>
  kind === "chatgpt"
    ? has
      ? "Signed in to a ChatGPT plan."
      : "Not signed in — connect this provider before adding models to it."
    : has
      ? "An API key is stored for this provider."
      : "No API key stored — add one before adding models to it.";

/** Why Add model is unavailable, or null when it is available. The server
 * refuses to build a provider without its credential, so a model added here
 * would fail the settings write anyway; saying so up front beats a rejection. */
const blockedReason = (p: ProviderView | undefined): string | null => {
  if (!p) return "Select a provider first.";
  if (p.hasCredential) return null;
  return p.kind === "chatgpt"
    ? `Connect “${p.name}” to a ChatGPT plan first.`
    : `Add an API key to “${p.name}” first.`;
};

type ProviderDraft = {
  name: string;
  kind: ProviderKind;
  baseUrl: string;
  apiKeyInput: string; // "" = leave the stored key unchanged
  hasCredential: boolean;
  keepThinkingSignature: boolean;
};

type ModelDraft = {
  alias: string;
  provider: string;
  modelId: string;
  maxTokens: string; // "" = unset
  contextWindow: string; // "" = unset (server applies a built-in default)
  thinkingEfforts: string[];
  thinkingEffort: string; // "" = no default
  thinkingDialect: string; // "" = no thinking control
  forcedToolsDisableThinking: boolean;
};

const providerToDraft = (p: ProviderView): ProviderDraft => ({
  name: p.name,
  kind: (PROVIDER_KINDS as string[]).includes(p.kind)
    ? (p.kind as ProviderKind)
    : "anthropic",
  baseUrl: p.baseUrl ?? "",
  apiKeyInput: "",
  hasCredential: p.hasCredential,
  keepThinkingSignature: p.keepThinkingSignature,
});

const modelToDraft = (m: ModelView): ModelDraft => ({
  alias: m.alias,
  provider: m.provider,
  modelId: m.modelId,
  maxTokens: m.maxTokens != null ? String(m.maxTokens) : "",
  contextWindow: m.contextWindow != null ? String(m.contextWindow) : "",
  thinkingEfforts: m.thinkingEfforts ?? [],
  thinkingEffort: m.thinkingEffort ?? "",
  thinkingDialect: m.thinkingDialect ?? "",
  forcedToolsDisableThinking: m.forcedToolsDisableThinking ?? false,
});

const newProvider = (): ProviderDraft => ({
  name: "",
  kind: "anthropic",
  baseUrl: "",
  apiKeyInput: "",
  hasCredential: false,
  keepThinkingSignature: false,
});

const newModel = (provider: string): ModelDraft => ({
  alias: "",
  provider,
  modelId: "",
  maxTokens: "",
  contextWindow: "",
  thinkingEfforts: [],
  thinkingEffort: "",
  thinkingDialect: "",
  forcedToolsDisableThinking: false,
});

const toProviderInput = (p: ProviderDraft): ProviderInput => ({
  name: p.name.trim(),
  kind: p.kind,
  baseUrl: p.baseUrl.trim() || undefined,
  apiKey: p.apiKeyInput === "" ? undefined : p.apiKeyInput,
  keepThinkingSignature: p.keepThinkingSignature,
});

const toModelInput = (m: ModelDraft): ModelInput => ({
  alias: m.alias.trim(),
  provider: m.provider,
  modelId: m.modelId.trim(),
  maxTokens: m.maxTokens.trim() ? Number(m.maxTokens.trim()) : undefined,
  thinkingEfforts: m.thinkingEfforts.length ? m.thinkingEfforts : undefined,
  thinkingEffort: m.thinkingEffort || undefined,
  thinkingDialect: m.thinkingDialect || undefined,
  forcedToolsDisableThinking: m.forcedToolsDisableThinking,
  contextWindow: m.contextWindow.trim() ? Number(m.contextWindow.trim()) : undefined,
});

/**
 * Providers, and the model aliases routed through each of them.
 *
 * A list you can open, not two stacks of expanded forms: models belong to a
 * provider, and the flat version made you scroll past six fields to learn that
 * a second entry existed.
 *
 * Every action writes immediately. `SettingsUpdate` replaces whole
 * collections, so an edit sends the current providers *and* models arrays with
 * the one changed entry substituted — the same payload the batched Save used
 * to send, just at the moment you press the button. Sending only `models`
 * would replace `providers` with nothing, which is why both always go.
 */
export function ModelsSettings() {
  const { data: settings, isLoading, isError } = useSettings();
  const update = useUpdateSettings();

  const [selected, setSelected] = useState<string | null>(null);
  const [editingProvider, setEditingProvider] = useState<string | null>(null);
  const [addingProvider, setAddingProvider] = useState(false);
  const [editingModel, setEditingModel] = useState<string | null>(null);
  const [addingModel, setAddingModel] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
  /** The provider whose ChatGPT sign-in panel is expanded, if any. */
  const [signingIn, setSigningIn] = useState<string | null>(null);
  const refreshSettings = useRefreshSettings();

  const providers = useMemo(() => settings?.providers ?? [], [settings]);
  const models = useMemo(() => settings?.models ?? [], [settings]);

  // Hold a selection whenever anything exists, so the detail half is never an
  // unexplained blank.
  useEffect(() => {
    if (providers.length === 0) setSelected(null);
    else if (!selected || !providers.some((p) => p.name === selected))
      setSelected(providers[0].name);
  }, [providers, selected]);

  const providerModels = models.filter((m) => m.provider === selected);
  const addModelBlocked = blockedReason(providers.find((p) => p.name === selected));

  const commit = (next: { providers?: ProviderInput[]; models?: ModelInput[] }) =>
    update.mutateAsync({
      providers: next.providers ?? providers.map(providerToDraft).map(toProviderInput),
      models: next.models ?? models.map(modelToDraft).map(toModelInput),
    });

  const saveProvider = async (draft: ProviderDraft, original: string | null) => {
    setLocalError(null);
    const name = draft.name.trim();
    if (!name) return setLocalError("Every provider needs a name.");
    if (providers.some((p) => p.name === name && p.name !== original))
      return setLocalError(`A provider named “${name}” already exists.`);

    const current = providers.map(providerToDraft);
    const next =
      original === null
        ? [...current, draft]
        : current.map((p) => (p.name === original ? draft : p));

    // A rename carries its models with it, or they point at a provider that no
    // longer exists and every session using them fails to start.
    const renamed =
      original !== null && original !== name
        ? models
            .map(modelToDraft)
            .map((m) => (m.provider === original ? { ...m, provider: name } : m))
        : undefined;

    try {
      await commit({
        providers: next.map(toProviderInput),
        models: renamed?.map(toModelInput),
      });
      setSelected(name);
      setEditingProvider(null);
      setAddingProvider(false);
      // A ChatGPT provider that has just been created cannot do anything until
      // it is signed in, and the sign-in needs a saved provider to attach to.
      // Opening it here is what makes that one pass instead of two.
      if (draft.kind === "chatgpt" && !draft.hasCredential) setSigningIn(name);
    } catch (e) {
      setLocalError(e instanceof ApiRequestError ? e.message : "Save failed.");
    }
  };

  const deleteProvider = async (name: string) => {
    setLocalError(null);
    // The server would accept this and leave the models dangling, so the guard
    // lives here rather than nowhere.
    const orphans = models.filter((m) => m.provider === name);
    if (orphans.length > 0) {
      const many = orphans.length !== 1;
      return setLocalError(
        `“${name}” still has ${many ? "models" : "a model"} routed through it: ${orphans
          .map((m) => m.alias)
          .join(", ")}. Delete or move ${many ? "them" : "it"} first.`,
      );
    }
    if (!confirm(`Delete provider “${name}”?`)) return;
    try {
      await commit({
        providers: providers
          .filter((p) => p.name !== name)
          .map(providerToDraft)
          .map(toProviderInput),
      });
    } catch (e) {
      setLocalError(e instanceof ApiRequestError ? e.message : "Delete failed.");
    }
  };

  const saveModel = async (draft: ModelDraft, original: string | null) => {
    setLocalError(null);
    const alias = draft.alias.trim();
    if (!alias) return setLocalError("Every model needs an alias.");
    if (models.some((m) => m.alias === alias && m.alias !== original))
      return setLocalError(`A model aliased “${alias}” already exists.`);
    for (const [label, v] of [
      ["Max tokens", draft.maxTokens],
      ["Context window", draft.contextWindow],
    ] as const) {
      if (v.trim() && !/^\d+$/.test(v.trim()))
        return setLocalError(`${label} for “${alias}” must be a number.`);
    }

    const current = models.map(modelToDraft);
    const next =
      original === null
        ? [...current, draft]
        : current.map((m) => (m.alias === original ? draft : m));
    try {
      await commit({ models: next.map(toModelInput) });
      setEditingModel(null);
      setAddingModel(false);
    } catch (e) {
      setLocalError(e instanceof ApiRequestError ? e.message : "Save failed.");
    }
  };

  const deleteModel = async (alias: string) => {
    setLocalError(null);
    if (!confirm(`Delete model “${alias}”?`)) return;
    try {
      await commit({
        models: models
          .filter((m) => m.alias !== alias)
          .map(modelToDraft)
          .map(toModelInput),
      });
    } catch (e) {
      setLocalError(e instanceof ApiRequestError ? e.message : "Delete failed.");
    }
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
        desc="API endpoints and the model aliases sessions pick from. Changes save as you make them."
        saving={update.isPending}
        saved={update.isSuccess && !update.isPending}
      />

      <SettingsPane>
        {isLoading && (
          <div className="py-16 text-center text-sm text-faint">Loading…</div>
        )}
        {isError && (
          <div className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
            Couldn’t load settings. Is <code>horsie serve</code> running?
          </div>
        )}

        {(localError || saveError) && (
          <div
            className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink"
            data-testid="models-error"
          >
            {localError ?? saveError}
          </div>
        )}

        {settings && (
          <>
            <Section
              title="Providers"
              desc="API endpoints. Select one to see the models routed through it."
              onAdd={() => {
                setAddingProvider(true);
                setEditingProvider(null);
              }}
              addLabel="Add provider"
              empty={
                providers.length === 0 && !addingProvider ? "No providers yet." : null
              }
            >
              {addingProvider && (
                <ProviderEditor
                  initial={newProvider()}
                  onSave={(d) => saveProvider(d, null)}
                  onCancel={() => setAddingProvider(false)}
                  busy={update.isPending}
                />
              )}
              {providers.map((p) => {
                const count = models.filter((m) => m.provider === p.name).length;
                return editingProvider === p.name ? (
                  <ProviderEditor
                    key={p.name}
                    initial={providerToDraft(p)}
                    onSave={(d) => saveProvider(d, p.name)}
                    onCancel={() => setEditingProvider(null)}
                    busy={update.isPending}
                  />
                ) : (
                  <ListRow
                    key={p.name}
                    testId={`provider-row-${p.name}`}
                    title={p.name}
                    subtitle={p.baseUrl || defaultEndpoint(p.kind)}
                    active={selected === p.name}
                    onActivate={() => setSelected(p.name)}
                    meta={
                      <span className="flex shrink-0 items-center gap-2">
                        {/* Whether this provider can authenticate is the one
                            thing here that decides if it works at all, so it is
                            a lamp and a word rather than a detail in the
                            editor. */}
                        <span
                          className={
                            p.hasCredential
                              ? "flex items-center gap-1.5 text-lamp-ok"
                              : "flex items-center gap-1.5 text-amber-ink"
                          }
                          title={credentialHint(p.kind, p.hasCredential)}
                        >
                          <span
                            className={p.hasCredential ? "lamp" : "lamp lamp-off"}
                            aria-hidden
                          />
                          <span className="legend text-current">
                            {credentialWords(p.kind, p.hasCredential)}
                          </span>
                        </span>
                        <span className="chip">
                          {KIND_LABELS[p.kind as ProviderKind] ?? p.kind}
                        </span>
                        <span className="legend">
                          {count} {count === 1 ? "model" : "models"}
                        </span>
                      </span>
                    }
                    actions={
                      <>
                        {p.kind === "chatgpt" &&
                          (p.hasCredential ? (
                            <RowAction
                              icon={<Plug size={14} />}
                              label={`ChatGPT sign-in for ${p.name}`}
                              pressed={signingIn === p.name}
                              onClick={() =>
                                setSigningIn(signingIn === p.name ? null : p.name)
                              }
                              testId={`provider-connect-${p.name}`}
                            />
                          ) : (
                            // Named and worded, not an icon: this is the one
                            // thing standing between a new ChatGPT provider and
                            // a working one.
                            <button
                              className="key key-go shrink-0"
                              onClick={() =>
                                setSigningIn(signingIn === p.name ? null : p.name)
                              }
                              data-testid={`provider-connect-${p.name}`}
                            >
                              <Plug size={13} aria-hidden /> Connect
                            </button>
                          ))}
                        <RowAction
                          icon={<Pencil size={14} />}
                          label={`Edit ${p.name}`}
                          onClick={() => {
                            setEditingProvider(p.name);
                            setAddingProvider(false);
                          }}
                          testId={`provider-edit-${p.name}`}
                        />
                        <RowAction
                          icon={<Trash2 size={14} />}
                          label={`Delete ${p.name}`}
                          danger
                          // Every write sends the whole collection, rebuilt
                          // from this render's data. Two deletes issued before
                          // the refetch lands would have the second resurrect
                          // the first.
                          disabled={update.isPending}
                          onClick={() => deleteProvider(p.name)}
                          testId={`provider-delete-${p.name}`}
                        />
                      </>
                    }
                  >
                    {signingIn === p.name && (
                      <ChatGptSignIn
                        provider={p.name}
                        onChanged={() => refreshSettings()}
                      />
                    )}
                  </ListRow>
                );
              })}
            </Section>

            {selected && (
              <Section
                title={`Models · ${selected}`}
                desc="Aliases sessions pick from. Each routes to a model id on this provider."
                onAdd={() => {
                  setAddingModel(true);
                  setEditingModel(null);
                }}
                addLabel="Add model"
                addDisabled={addModelBlocked !== null}
                addTitle={addModelBlocked ?? undefined}
                empty={
                  providerModels.length === 0 && !addingModel
                    ? // The reason goes here as well as in the tooltip: a
                      // disabled button with a hover-only explanation is a dead
                      // end on touch and for a keyboard.
                      (addModelBlocked ?? `No models route through ${selected} yet.`)
                    : null
                }
              >
                {addingModel && (
                  <ModelEditor
                    initial={newModel(selected)}
                    providerNames={providers.map((p) => p.name)}
                    onSave={(d) => saveModel(d, null)}
                    onCancel={() => setAddingModel(false)}
                    busy={update.isPending}
                  />
                )}
                {providerModels.map((m) =>
                  editingModel === m.alias ? (
                    <ModelEditor
                      key={m.alias}
                      initial={modelToDraft(m)}
                      providerNames={providers.map((p) => p.name)}
                      onSave={(d) => saveModel(d, m.alias)}
                      onCancel={() => setEditingModel(null)}
                      busy={update.isPending}
                    />
                  ) : (
                    <ListRow
                      key={m.alias}
                      testId={`model-row-${m.alias}`}
                      title={m.alias}
                      subtitle={m.modelId}
                      meta={
                        m.thinkingEfforts && m.thinkingEfforts.length > 0 ? (
                          <span className="chip shrink-0">thinking</span>
                        ) : undefined
                      }
                      actions={
                        <>
                          <RowAction
                            icon={<Pencil size={14} />}
                            label={`Edit ${m.alias}`}
                            onClick={() => {
                              setEditingModel(m.alias);
                              setAddingModel(false);
                            }}
                            testId={`model-edit-${m.alias}`}
                          />
                          <RowAction
                            icon={<Trash2 size={14} />}
                            label={`Delete ${m.alias}`}
                            danger
                            disabled={update.isPending}
                            onClick={() => deleteModel(m.alias)}
                            testId={`model-delete-${m.alias}`}
                          />
                        </>
                      }
                    />
                  ),
                )}
              </Section>
            )}
          </>
        )}
      </SettingsPane>
    </div>
  );
}

function defaultEndpoint(kind: string): string {
  if (kind === "openai") return "OpenAI-compatible endpoint";
  return KIND_PLACEHOLDERS[kind as ProviderKind] ?? "https://api.anthropic.com";
}

/** The shell both entity forms sit in. Each commits on its own now, so each
 * carries its own Save and Cancel. */
function Editor({
  children,
  onSave,
  onCancel,
  busy,
  saveLabel,
  testId,
}: {
  children: React.ReactNode;
  onSave: () => void;
  onCancel: () => void;
  busy: boolean;
  saveLabel: string;
  testId: string;
}) {
  return (
    <div
      className="rounded-[var(--radius-control)] bg-raised p-3 shadow-[inset_0_0_0_1px_var(--rule-strong)]"
      data-testid={testId}
    >
      {children}
      <div className="mt-3 flex items-center gap-2">
        <button
          className="key key-go"
          onClick={onSave}
          disabled={busy}
          data-testid="editor-save"
        >
          {busy ? "Saving…" : saveLabel}
        </button>
        <button className="key key-flat" onClick={onCancel} data-testid="editor-cancel">
          <X size={13} aria-hidden /> Cancel
        </button>
      </div>
    </div>
  );
}

function ProviderEditor({
  initial,
  onSave,
  onCancel,
  busy,
}: {
  initial: ProviderDraft;
  onSave: (d: ProviderDraft) => void;
  onCancel: () => void;
  busy: boolean;
}) {
  const [draft, setDraft] = useState(initial);
  const set = (patch: Partial<ProviderDraft>) => setDraft((d) => ({ ...d, ...patch }));
  return (
    <Editor
      onSave={() => onSave(draft)}
      onCancel={onCancel}
      busy={busy}
      saveLabel="Save provider"
      testId="provider-editor"
    >
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <TextField
          label="Name"
          value={draft.name}
          onChange={(v) => set({ name: v })}
          placeholder="anthropic"
        />
        <label className="block">
          <RowLabel>Kind</RowLabel>
          <select
            className="field font-mono"
            value={draft.kind}
            onChange={(e) => set({ kind: e.target.value as ProviderKind })}
          >
            {PROVIDER_KINDS.map((k) => (
              <option key={k} value={k}>
                {KIND_LABELS[k]}
              </option>
            ))}
          </select>
        </label>
        <TextField
          label="Base URL (optional)"
          value={draft.baseUrl}
          onChange={(v) => set({ baseUrl: v })}
          placeholder={KIND_PLACEHOLDERS[draft.kind]}
        />
        {usesApiKey(draft.kind) && (
          <TextField
            label="Inline key"
            type="password"
            value={draft.apiKeyInput}
            onChange={(v) => set({ apiKeyInput: v })}
            placeholder={draft.hasCredential ? "•••• stored — blank keeps it" : "not set"}
          />
        )}
        {draft.kind === "chatgpt" && (
          <p className="col-span-1 text-xs text-dim sm:col-span-2">
            A ChatGPT plan is authorized by signing in, not by a key. Connect it
            from its row in the list{initial.name ? "" : " once this is saved"}.
          </p>
        )}
        {draft.kind === "anthropic" && (
          <label className="col-span-1 flex items-start gap-2 text-sm sm:col-span-2">
            <input
              type="checkbox"
              className="mt-1"
              checked={draft.keepThinkingSignature}
              onChange={(e) => set({ keepThinkingSignature: e.target.checked })}
            />
            <span>
              Keep thinking signatures
              <span className="block text-xs text-dim">
                Required for api.anthropic.com, which validates them on replay.
                Leave off for Anthropic-compatible endpoints — the blobs are
                several KB per thinking block and nothing reads them.
              </span>
            </span>
          </label>
        )}
      </div>
    </Editor>
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
      thinkingEfforts:
        draft.thinkingEfforts.length === 0 && card.thinkingEfforts != null
          ? card.thinkingEfforts
          : draft.thinkingEfforts,
      thinkingEffort:
        draft.thinkingEffort === "" && card.defaultThinkingEffort != null
          ? card.defaultThinkingEffort
          : draft.thinkingEffort,
      thinkingDialect:
        draft.thinkingDialect === "" && card.thinkingDialect != null
          ? card.thinkingDialect
          : draft.thinkingDialect,
      // Same "only fill what is still empty" rule as the fields above; for a
      // boolean, unset is `false`. The card's `baseUrl` is deliberately not
      // read here — it describes the provider, not the model.
      forcedToolsDisableThinking:
        draft.forcedToolsDisableThinking || (card.forcedToolsDisableThinking ?? false),
    });
    setFocused(false);
  };

  return (
    <label className="relative block">
      <RowLabel>Model id</RowLabel>
      <input
        className="field font-mono"
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
          className="absolute z-10 mt-1 max-h-48 w-full overflow-y-auto rounded-[var(--radius-control)] border shadow-lg"
          style={{ background: "var(--panel)" }}
          data-testid="model-card-suggestions"
        >
          {suggestions!.map((c) => (
            <li key={c.modelId}>
              <button
                type="button"
                className="flex w-full items-baseline justify-between gap-2 px-2.5 py-1.5 text-left text-xs hover:bg-raised"
                onMouseDown={(e) => {
                  e.preventDefault();
                  pick(c);
                }}
                data-testid={`model-card-suggestion-${c.modelId}`}
              >
                <span className="font-mono text-legend">{c.modelId}</span>
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

function ModelEditor({
  initial,
  providerNames,
  onSave,
  onCancel,
  busy,
}: {
  initial: ModelDraft;
  providerNames: string[];
  onSave: (d: ModelDraft) => void;
  onCancel: () => void;
  busy: boolean;
}) {
  const [draft, setDraft] = useState(initial);
  const set = (patch: Partial<ModelDraft>) => setDraft((d) => ({ ...d, ...patch }));
  const options =
    draft.provider && !providerNames.includes(draft.provider)
      ? [draft.provider, ...providerNames]
      : providerNames;
  return (
    <Editor
      onSave={() => onSave(draft)}
      onCancel={onCancel}
      busy={busy}
      saveLabel="Save model"
      testId="model-editor"
    >
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <TextField
          label="Alias"
          value={draft.alias}
          onChange={(v) => set({ alias: v })}
          placeholder="sonnet"
        />
        <label className="block">
          <RowLabel>Provider</RowLabel>
          <select
            className="field font-mono"
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
        <div className="col-span-1 border-t pt-3 sm:col-span-2">
          <RowLabel>Thinking efforts this model offers</RowLabel>
          <div className="flex flex-wrap gap-3">
            {EFFORTS.map((e) => (
              <label key={e} className="flex items-center gap-1 text-sm">
                <input
                  type="checkbox"
                  checked={draft.thinkingEfforts.includes(e)}
                  onChange={(ev) => {
                    const next = ev.target.checked
                      ? [...draft.thinkingEfforts, e]
                      : draft.thinkingEfforts.filter((x) => x !== e);
                    const ordered = EFFORTS.filter((x) => next.includes(x)) as string[];
                    set({
                      thinkingEfforts: ordered,
                      // a default that is no longer offered would be rejected on save
                      thinkingEffort: ordered.includes(draft.thinkingEffort)
                        ? draft.thinkingEffort
                        : "",
                    });
                  }}
                />
                {e}
              </label>
            ))}
          </div>
          <div className="mt-2 grid grid-cols-1 gap-3 sm:grid-cols-2">
            <label className="block">
              <RowLabel>Default effort</RowLabel>
              <select
                className="field font-mono"
                value={draft.thinkingEffort}
                onChange={(ev) => set({ thinkingEffort: ev.target.value })}
              >
                <option value="">(none)</option>
                {draft.thinkingEfforts.map((e) => (
                  <option key={e} value={e}>
                    {e}
                  </option>
                ))}
              </select>
            </label>
            <label className="block">
              <RowLabel>Wire dialect</RowLabel>
              <select
                className="field font-mono"
                value={draft.thinkingDialect}
                onChange={(ev) => set({ thinkingDialect: ev.target.value })}
              >
                {DIALECTS.map((d) => (
                  <option key={d} value={d}>
                    {d === "" ? "(none)" : d}
                  </option>
                ))}
              </select>
            </label>
          </div>
          <label className="mt-2 flex items-start gap-2 text-sm">
            <input
              type="checkbox"
              className="mt-1"
              checked={draft.forcedToolsDisableThinking}
              onChange={(ev) => set({ forcedToolsDisableThinking: ev.target.checked })}
              data-testid="model-forced-tools"
            />
            <span>
              Pinned tool choice disables thinking
              <span className="block text-xs text-dim">
                Required for DeepSeek, which rejects a forced tool choice while
                thinking is on. Sub-agents that must call a handoff tool will
                run without thinking.
              </span>
            </span>
          </label>
        </div>
      </div>
    </Editor>
  );
}
