import { Loader2, Plus, Save, Trash2 } from "lucide-react";
import { useState } from "react";
import { ApiRequestError } from "../../api/client";
import type { ModelCard } from "../../api/types";
import {
  useAdminModelCards,
  useCreateModelCard,
  useDeleteModelCard,
  useUpdateModelCard,
} from "../../hooks/useModelCards";
import { RowLabel } from "../settings/fields";
import { SettingsHeader } from "../settings/SettingsHeader";

export function ModelCardsPage() {
  return (
    <div className="flex h-full flex-col overflow-hidden">
      <SettingsHeader
        title="Model cards"
        desc="Well-known models and their token limits, offered as autocomplete in Settings."
      />
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-3xl space-y-6 px-6 py-6">
          <ModelCardsSection />
        </div>
      </div>
    </div>
  );
}

function ModelCardsSection() {
  const { data: cards, isLoading, isError } = useAdminModelCards();
  const [adding, setAdding] = useState(false);
  return (
    <section className="card p-4">
      <div className="mb-3 flex items-start justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold text-text">Model cards</h2>
          <p className="mt-0.5 text-xs text-faint">
            Well-known models and their token limits. The Settings model form
            autocompletes model ids from these and prefills empty limit
            fields — editing a card never changes already-configured models.
          </p>
        </div>
        <button
          className="btn-outline shrink-0 !px-2.5 !py-1.5 text-xs"
          onClick={() => setAdding(true)}
          data-testid="add-model-card"
        >
          <Plus size={14} /> Add card
        </button>
      </div>
      <div className="space-y-2.5">
        {isLoading && <p className="text-sm text-faint">Loading…</p>}
        {isError && (
          <p className="text-sm text-error">Couldn’t load model cards.</p>
        )}
        {cards?.length === 0 && !adding && (
          <p className="rounded-[var(--radius)] border border-dashed px-3 py-4 text-center text-sm text-faint">
            No model cards.
          </p>
        )}
        {adding && <ModelCardRow onDone={() => setAdding(false)} />}
        {cards?.map((c) => <ModelCardRow key={c.modelId} card={c} />)}
      </div>
    </section>
  );
}

/** One card row for both a new (unsaved) and an existing card. Save creates
 * or updates immediately; Remove deletes (or drops the new draft). The model
 * id is the id of record, so it is fixed once saved. */
function ModelCardRow({
  card,
  onDone,
}: {
  card?: ModelCard;
  onDone?: () => void;
}) {
  const create = useCreateModelCard();
  const update = useUpdateModelCard();
  const remove = useDeleteModelCard();
  const isNew = !card;

  const [modelId, setModelId] = useState(card?.modelId ?? "");
  const [name, setName] = useState(card?.name ?? "");
  const [contextWindow, setContextWindow] = useState(
    card?.contextWindow != null ? String(card.contextWindow) : "",
  );
  const [maxTokens, setMaxTokens] = useState(
    card?.maxTokens != null ? String(card.maxTokens) : "",
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
        });
        onDone?.();
      } else {
        await update.mutateAsync({
          modelId: card.modelId,
          body: {
            name: name.trim(),
            contextWindow: parseNum(contextWindow),
            maxTokens: parseNum(maxTokens),
          },
        });
        setDirty(false);
      }
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Save failed.");
    }
  };

  const onRemove = async () => {
    setError(null);
    if (isNew) return onDone?.();
    if (
      !confirm(
        `Delete model card "${card.modelId}"? Models already configured keep their current values.`,
      )
    )
      return;
    try {
      await remove.mutateAsync(card.modelId);
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Delete failed.");
    }
  };

  const pending = create.isPending || update.isPending || remove.isPending;

  return (
    <div
      className="rounded-[var(--radius)] border p-3"
      style={{ background: "var(--surface-2)" }}
      data-testid={isNew ? "model-card-row-new" : `model-card-row-${card.modelId}`}
    >
      <div className="grid grid-cols-2 gap-3">
        <label className="block">
          <RowLabel>Model id</RowLabel>
          <input
            className="input font-mono"
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
            className="input"
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
            className="input font-mono"
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
            className="input font-mono"
            value={maxTokens}
            onChange={(e) => {
              setMaxTokens(e.target.value);
              touch();
            }}
            placeholder="16384"
          />
        </label>
      </div>

      {error && (
        <div className="mt-3 rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
          {error}
        </div>
      )}

      <div className="mt-3 flex items-center justify-end gap-2">
        <button
          className="btn-icon text-faint hover:text-error"
          onClick={onRemove}
          aria-label={isNew ? "Discard new card" : "Delete card"}
          data-testid="model-card-remove"
          disabled={pending}
        >
          <Trash2 size={15} />
        </button>
        <button
          className="btn-primary !px-2.5 !py-1.5 text-xs"
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
