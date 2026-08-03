import { Info, Loader2, Pencil, Save, Trash2, X } from "lucide-react";
import { useState } from "react";
import { ApiRequestError } from "../../api/client";
import type { ModelCard } from "../../api/types";
import {
  useAdminModelCards,
  useCreateModelCard,
  useDeleteModelCard,
  useUpdateModelCard,
} from "../../hooks/useModelCards";
import { compactNumber } from "../../lib/format";
import { ListRow, RowAction, RowLabel, Section, SettingsPane } from "../settings/fields";
import { SettingsHeader } from "../settings/SettingsHeader";

export function ModelCardsPage() {
  return (
    <div className="flex h-full flex-col overflow-hidden">
      <SettingsHeader
        title="Model cards"
        desc="Well-known models and their token limits. Settings → Models autocompletes model ids from these and prefills empty limit fields; editing a card never changes an already-configured model."
      />
      <SettingsPane>
        <ModelCardsSection />
      </SettingsPane>
    </div>
  );
}

/**
 * The catalog as a catalog.
 *
 * Every card used to render as a fully-expanded six-field form, so a seeded
 * install opened on twenty stacked forms and answering "is DeepSeek in here?"
 * meant scrolling through all of them. A row now states what the card is —
 * name, model id, and the limits that are the reason to look it up — and the
 * form opens on request.
 */
function ModelCardsSection() {
  const { data: cards, isLoading, isError } = useAdminModelCards();
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);

  return (
    <Section
      title="Catalog"
      desc="One entry per well-known model."
      onAdd={() => {
        setAdding(true);
        setEditing(null);
      }}
      addLabel="Add card"
      addTestId="add-model-card"
      empty={cards?.length === 0 && !adding ? "No model cards." : null}
    >
      {isLoading && <p className="text-sm text-faint">Loading…</p>}
      {isError && <p className="text-sm text-red-ink">Couldn’t load model cards.</p>}

      {adding && <ModelCardEditor onDone={() => setAdding(false)} />}

      {cards?.map((c) =>
        editing === c.modelId ? (
          <ModelCardEditor
            key={c.modelId}
            card={c}
            onDone={() => setEditing(null)}
          />
        ) : (
          <ListRow
            key={c.modelId}
            testId={`model-card-row-${c.modelId}`}
            title={c.name}
            subtitle={c.modelId}
            meta={
              <span className="hidden shrink-0 items-center gap-2 sm:flex">
                {c.contextWindow != null && (
                  <span className="legend">
                    {compactNumber(c.contextWindow)} ctx
                  </span>
                )}
                {c.maxTokens != null && (
                  <span className="legend">
                    {compactNumber(c.maxTokens)} out
                  </span>
                )}
              </span>
            }
            actions={
              <>
                <RowAction
                  icon={<Info size={14} />}
                  label={`Details for ${c.name}`}
                  pressed={expanded === c.modelId}
                  onClick={() =>
                    setExpanded(expanded === c.modelId ? null : c.modelId)
                  }
                  testId={`model-card-info-${c.modelId}`}
                />
                <RowAction
                  icon={<Pencil size={14} />}
                  label={`Edit ${c.name}`}
                  onClick={() => {
                    setEditing(c.modelId);
                    setAdding(false);
                  }}
                  testId={`model-card-edit-${c.modelId}`}
                />
                <DeleteCardAction card={c} />
              </>
            }
          >
            {expanded === c.modelId && <CardDetails card={c} />}
          </ListRow>
        ),
      )}
    </Section>
  );
}

/** The read-only view behind the info key: everything the card carries that
 * the row has no room for. */
function CardDetails({ card }: { card: ModelCard }) {
  const rows: [string, string][] = [
    ["Model id", card.modelId],
    ["Context window", card.contextWindow != null ? card.contextWindow.toLocaleString() : "—"],
    ["Max tokens", card.maxTokens != null ? card.maxTokens.toLocaleString() : "—"],
    ["Base URL", card.baseUrl || "—"],
    ["Thinking dialect", card.thinkingDialect || "—"],
    [
      "Thinking efforts",
      card.thinkingEfforts?.length ? card.thinkingEfforts.join(", ") : "—",
    ],
    ["Default effort", card.defaultThinkingEffort || "—"],
    [
      "Pinned tool choice disables thinking",
      card.forcedToolsDisableThinking ? "Yes" : "No",
    ],
  ];
  return (
    <dl className="grid grid-cols-1 gap-x-6 gap-y-1.5 sm:grid-cols-2">
      {rows.map(([k, v]) => (
        <div key={k} className="flex items-baseline justify-between gap-3">
          <dt className="legend">{k}</dt>
          <dd className="min-w-0 truncate font-mono text-[11px] text-legend">{v}</dd>
        </div>
      ))}
    </dl>
  );
}

function DeleteCardAction({ card }: { card: ModelCard }) {
  const remove = useDeleteModelCard();
  return (
    <RowAction
      icon={<Trash2 size={14} />}
      label={`Delete ${card.name}`}
      danger
      disabled={remove.isPending}
      onClick={() => {
        if (
          !confirm(
            `Delete model card "${card.modelId}"? Models already configured keep their current values.`,
          )
        )
          return;
        remove.mutate(card.modelId);
      }}
      testId={`model-card-delete-${card.modelId}`}
    />
  );
}

/** The editor, for both a new (unsaved) and an existing card. Save creates or
 * updates immediately. The model id is the id of record, so it is fixed once
 * saved. */
function ModelCardEditor({
  card,
  onDone,
}: {
  card?: ModelCard;
  onDone: () => void;
}) {
  const create = useCreateModelCard();
  const update = useUpdateModelCard();
  const isNew = !card;

  const [modelId, setModelId] = useState(card?.modelId ?? "");
  const [name, setName] = useState(card?.name ?? "");
  const [contextWindow, setContextWindow] = useState(
    card?.contextWindow != null ? String(card.contextWindow) : "",
  );
  const [maxTokens, setMaxTokens] = useState(
    card?.maxTokens != null ? String(card.maxTokens) : "",
  );
  const [baseUrl, setBaseUrl] = useState(card?.baseUrl ?? "");
  const [forcedToolsDisableThinking, setForcedToolsDisableThinking] = useState(
    card?.forcedToolsDisableThinking ?? false,
  );
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const touch = () => setDirty(true);

  const parseNum = (s: string): number | undefined =>
    s.trim() === "" ? undefined : Number(s);

  const save = async () => {
    setError(null);
    if (isNew && !modelId.trim()) return setError("Model id is required.");
    if (!name.trim()) return setError("Name is required.");
    for (const [label, v] of [
      ["Context window", contextWindow],
      ["Max tokens", maxTokens],
    ] as const) {
      if (v.trim() !== "" && (!Number.isInteger(Number(v)) || Number(v) <= 0))
        return setError(`${label} must be a positive whole number.`);
    }
    try {
      if (isNew) {
        await create.mutateAsync({
          modelId: modelId.trim(),
          name: name.trim(),
          contextWindow: parseNum(contextWindow),
          maxTokens: parseNum(maxTokens),
          baseUrl: baseUrl.trim() || undefined,
          forcedToolsDisableThinking,
        });
        onDone?.();
      } else {
        await update.mutateAsync({
          modelId: card.modelId,
          body: {
            name: name.trim(),
            contextWindow: parseNum(contextWindow),
            maxTokens: parseNum(maxTokens),
            baseUrl: baseUrl.trim() || undefined,
            forcedToolsDisableThinking,
          },
        });
        setDirty(false);
        // Back to the row. The list is the resting state of this page now, so
        // an editor left open after a successful save is a page that looks
        // like it did nothing.
        onDone();
      }
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Save failed.");
    }
  };

  const pending = create.isPending || update.isPending;

  return (
    <div
      className="rounded-[var(--radius-control)] bg-raised p-3 shadow-[inset_0_0_0_1px_var(--rule-strong)]"
      data-testid={isNew ? "model-card-editor-new" : `model-card-editor-${card.modelId}`}
    >
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <label className="block">
          <RowLabel>Model id</RowLabel>
          <input
            className="field font-mono"
            value={modelId}
            onChange={(e) => {
              setModelId(e.target.value);
              touch();
            }}
            placeholder="claude-sonnet-4-6"
            disabled={!isNew}
          />
        </label>
        <label className="block">
          <RowLabel>Name</RowLabel>
          <input
            className="field"
            value={name}
            onChange={(e) => {
              setName(e.target.value);
              touch();
            }}
            placeholder="Claude Sonnet 4.6"
          />
        </label>
        <label className="block">
          <RowLabel>Context window (optional)</RowLabel>
          <input
            className="field font-mono"
            value={contextWindow}
            onChange={(e) => {
              setContextWindow(e.target.value);
              touch();
            }}
            placeholder="200000"
          />
        </label>
        <label className="block">
          <RowLabel>Max tokens (optional)</RowLabel>
          <input
            className="field font-mono"
            value={maxTokens}
            onChange={(e) => {
              setMaxTokens(e.target.value);
              touch();
            }}
            placeholder="16384"
          />
        </label>
        <label className="block">
          <RowLabel>Base URL (optional)</RowLabel>
          <input
            className="field font-mono"
            value={baseUrl}
            onChange={(e) => {
              setBaseUrl(e.target.value);
              touch();
            }}
            placeholder="https://api.deepseek.com"
            data-testid="model-card-base-url"
          />
        </label>
        <label className="col-span-1 sm:col-span-2 flex items-start gap-2 text-sm">
          <input
            type="checkbox"
            className="mt-1"
            checked={forcedToolsDisableThinking}
            onChange={(e) => {
              setForcedToolsDisableThinking(e.target.checked);
              touch();
            }}
            data-testid="model-card-forced-tools"
          />
          <span>
            Pinned tool choice disables thinking
            <span className="block text-xs text-dim">
              For backends that reject a forced <code>tool_choice</code> while
              thinking is on — DeepSeek answers 400 “Thinking mode does not
              support this tool_choice”.
            </span>
          </span>
        </label>
      </div>

      {error && (
        <div className="mt-3 rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
          {error}
        </div>
      )}

      <div className="mt-3 flex items-center justify-end gap-2">
        <button
          className="key key-flat"
          onClick={onDone}
          data-testid="model-card-cancel"
          disabled={pending}
        >
          <X size={13} aria-hidden /> Cancel
        </button>
        <button
          className="key key-go"
          onClick={save}
          disabled={(!isNew && !dirty) || pending}
          data-testid="model-card-save"
        >
          {pending ? (
            <Loader2 size={14} className="animate-spin" />
          ) : (
            <Save size={14} />
          )}
          {isNew ? "Add card" : "Save"}
        </button>
      </div>
    </div>
  );
}
