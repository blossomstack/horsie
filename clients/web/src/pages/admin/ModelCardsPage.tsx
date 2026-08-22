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
import { ListRow, RowAction, RowLabel, Section, SettingsPage } from "../settings/fields";
import { askConfirm } from "../../lib/confirm";

export function ModelCardsPage() {
  return (
    <SettingsPage
        title="Model cards"
        desc="Well-known models and their token limits. Settings → Models autocompletes model ids from these and prefills empty limit fields; editing a card never changes an already-configured model."
    >
        <ModelCardsSection />
      </SettingsPage>
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
  const [filter, setFilter] = useState("");

  // A seeded catalog is 44 rows. Answering "is DeepSeek in here?" by scrolling
  // is the thing the row-not-form redesign was already trying to fix.
  const needle = filter.trim().toLowerCase();
  const shown = needle
    ? (cards ?? []).filter((c) =>
        `${c.modelId} ${c.name}`.toLowerCase().includes(needle),
      )
    : (cards ?? []);

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

      {(cards?.length ?? 0) > 8 && (
        <input
          className="field"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") setFilter("");
          }}
          placeholder="Filter by model id or name…"
          aria-label="Filter model cards"
          data-testid="model-card-filter"
        />
      )}
      {needle !== "" && shown.length === 0 && (
        <p className="screen px-3 py-5 text-center text-sm text-faint">
          No card matches “{filter.trim()}”.
        </p>
      )}

      {adding && <ModelCardEditor onDone={() => setAdding(false)} />}

      {shown.map((c) =>
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
            // `.item-title` is the mono face for machine strings, so the id
            // takes it and the display name reads as the prose it is. The
            // other way round put mono on "Claude Opus 4.6" and sans on
            // `claude-opus-4-6`.
            title={c.modelId}
            subtitle={c.name}
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
          <dd className="min-w-0 truncate font-mono text-[0.6875rem] text-legend">{v}</dd>
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
      onClick={async () => {
        if (
          !(await askConfirm(
            `Delete model card "${card.modelId}"? Models already configured keep their current values.`,
          ))
        )
          return;
        remove.mutate(card.modelId);
      }}
      testId={`model-card-delete-${card.modelId}`}
    />
  );
}

/**
 * The canonical thinking values, mirrored from `crates/agentcore/src/thinking.rs`
 * (`ThinkingEffort::parse` and `ThinkingDialect::parse`). A card carrying
 * anything else is data the server should never have accepted, so the editor
 * shows it rather than silently dropping it — losing it on save is the bug this
 * whole change exists to stop.
 */
const THINKING_EFFORTS = [
  "none",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
] as const;

const THINKING_DIALECTS = [
  "anthropic_effort",
  "anthropic_always_on",
  "anthropic_budget",
  "openai_effort",
  "zai_thinking",
  "kimi_thinking",
  "none",
] as const;

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
  const [thinkingEfforts, setThinkingEfforts] = useState<string[]>(
    card?.thinkingEfforts ?? [],
  );
  const [defaultThinkingEffort, setDefaultThinkingEffort] = useState(
    card?.defaultThinkingEffort ?? "",
  );
  const [thinkingDialect, setThinkingDialect] = useState(
    card?.thinkingDialect ?? "",
  );
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const touch = () => setDirty(true);

  // A stored value the canonical list does not contain still gets an option,
  // so opening and saving a card cannot quietly discard it.
  const effortOptions = [
    ...THINKING_EFFORTS,
    ...thinkingEfforts.filter(
      (e) => !THINKING_EFFORTS.includes(e as (typeof THINKING_EFFORTS)[number]),
    ),
  ];
  const dialectOptions = [
    ...THINKING_DIALECTS,
    ...(thinkingDialect &&
    !THINKING_DIALECTS.includes(
      thinkingDialect as (typeof THINKING_DIALECTS)[number],
    )
      ? [thinkingDialect]
      : []),
  ];

  const toggleEffort = (effort: string) => {
    setThinkingEfforts((current) =>
      current.includes(effort)
        ? current.filter((e) => e !== effort)
        : // Kept in canonical order, which the wire type documents as
          // ascending — a card is read as a range, not a set.
          effortOptions.filter((e) => e === effort || current.includes(e)),
    );
    touch();
  };

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
          thinkingEfforts: thinkingEfforts.length ? thinkingEfforts : undefined,
          defaultThinkingEffort: defaultThinkingEffort || undefined,
          thinkingDialect: thinkingDialect || undefined,
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
            thinkingEfforts: thinkingEfforts.length ? thinkingEfforts : undefined,
            defaultThinkingEffort: defaultThinkingEffort || undefined,
            thinkingDialect: thinkingDialect || undefined,
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

        {/* The three fields the editor could not show. A full-replacement PUT
          plus a partial form meant every save stripped them, and
          `seed_if_missing` never repairs an existing row — so one operator
          bumping a max-token count permanently destroyed a model's thinking
          config with no way to put it back in the product. */}
        <fieldset className="col-span-1 sm:col-span-2">
          <RowLabel>Thinking efforts (optional)</RowLabel>
          <div className="mt-1 flex flex-wrap gap-x-4 gap-y-2">
            {effortOptions.map((effort) => (
              <label
                key={effort}
                className="flex items-center gap-1.5 text-sm text-dim"
              >
                <input
                  type="checkbox"
                  checked={thinkingEfforts.includes(effort)}
                  onChange={() => toggleEffort(effort)}
                  data-testid={`model-card-effort-${effort}`}
                />
                <span className="font-mono text-xs">{effort}</span>
              </label>
            ))}
          </div>
          <span className="mt-1 block text-xs text-dim">
            What this model accepts, ascending. Leave empty for a model with no
            thinking control.
          </span>
        </fieldset>
        <label className="block">
          <RowLabel>Default thinking effort (optional)</RowLabel>
          <select
            className="field font-mono"
            value={defaultThinkingEffort}
            onChange={(e) => {
              setDefaultThinkingEffort(e.target.value);
              touch();
            }}
            data-testid="model-card-default-effort"
          >
            <option value="">—</option>
            {effortOptions.map((effort) => (
              <option key={effort} value={effort}>
                {effort}
              </option>
            ))}
          </select>
        </label>
        <label className="block">
          <RowLabel>Thinking dialect (optional)</RowLabel>
          <select
            className="field font-mono"
            value={thinkingDialect}
            onChange={(e) => {
              setThinkingDialect(e.target.value);
              touch();
            }}
            data-testid="model-card-dialect"
          >
            <option value="">—</option>
            {dialectOptions.map((dialect) => (
              <option key={dialect} value={dialect}>
                {dialect}
              </option>
            ))}
          </select>
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
