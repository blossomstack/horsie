import { ChevronRight, Loader2, RotateCcw, Trash2, Webhook } from "lucide-react";
import { useState } from "react";
import { askConfirm } from "../../../lib/confirm";
import type {
  CatalogEntryView,
  PluginKind,
  PluginView,
} from "../../../api/types";
import { cn } from "../../../lib/cn";
import {
  useRemovePlugin,
  useSetPluginDefault,
  useUpdatePlugin,
} from "../../../hooks/usePlugins";
import { useTranslation } from "react-i18next";

/** What a user types to reach an entry. Agents answer to `@`, the rest to `/`. */
export function sigilFor(kind: string): string {
  return kind === "agent" ? "@" : "/";
}

/**
 * `2 commands · 1 skill`, in catalogue order, with the empty kinds left out.
 *
 * An empty authored bundle gets its own wording. It is the state every
 * authored plugin starts in and is expected to sit in while it is being
 * filled, so reporting it the way a clone with nothing in it is reported
 * would read as a fault rather than as a beginning.
 */
function summarise(catalog: CatalogEntryView[], kind: PluginKind): string {
  const counts: [string, string][] = [
    ["command", "command"],
    ["skill", "skill"],
    ["agent", "agent"],
  ];
  const parts = counts
    .map(([kind, noun]) => {
      const n = catalog.filter((e) => e.kind === kind).length;
      return n === 0 ? null : `${n} ${noun}${n === 1 ? "" : "s"}`;
    })
    .filter((p): p is string => p !== null);
  if (parts.length > 0) return parts.join(" · ");
  return kind.kind === "Authored"
    ? "no skills written yet"
    : "nothing horsie runs";
}

/** What a bundle is, in the fewest words that still distinguish the three. */
function kindLabel(kind: PluginKind): string {
  switch (kind.kind) {
    case "Claude":
      return "claude";
    case "AgentPlugin":
      return "agent-plugin";
    case "Authored":
      return "authored here";
  }
}

function marketplaceOf(kind: PluginKind): string | undefined {
  return kind.kind === "Authored"
    ? undefined
    : (kind.value.marketplace ?? undefined);
}

export function BundleRow({ bundle }: { bundle: PluginView }) {
  const { t } = useTranslation();
  const setDefault = useSetPluginDefault();
  const update = useUpdatePlugin();
  const remove = useRemovePlugin();
  const [open, setOpen] = useState(false);
  const catalog = bundle.catalog ?? [];

  return (
    <div
      className="rounded-[var(--radius-control)] p-3"
      style={{ background: "var(--panel-raised)" }}
      data-testid="bundle-row"
    >
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="item-title truncate">{bundle.name}</span>
            {bundle.version && (
              <span className="chip !py-0 text-[0.625rem]">
                {bundle.version}
              </span>
            )}
            {/* What it is, so an authored bundle is not indistinguishable
                from a clone — and so a portable one can say so. */}
            <span className="chip !py-0 text-[0.625rem]">
              {kindLabel(bundle.kind)}
            </span>
            {/* Where it came from, so a bundle installed through a catalogue is
                not indistinguishable from one pasted by URL. */}
            {marketplaceOf(bundle.kind) && (
              <span className="chip !py-0 text-[0.625rem]">
                {marketplaceOf(bundle.kind)}
              </span>
            )}
            {bundle.hasHooks && (
              <span className="chip !py-0 flex items-center gap-1 text-[0.625rem]">
                <Webhook size={11} /> {t("skills.hooks")}
              </span>
            )}
          </div>
          {bundle.description && (
            <p className="mt-0.5 text-xs text-dim">{bundle.description}</p>
          )}
          {/* The counts are the disclosure: what a bundle *offers* is the
              question this page exists to answer, and the entries below are
              the exact strings to type. */}
          <button
            type="button"
            className="mt-0.5 flex items-center gap-1 text-[0.6875rem] text-faint hover:text-dim"
            aria-expanded={open}
            onClick={() => setOpen((v) => !v)}
            disabled={catalog.length === 0}
          >
            <ChevronRight
              size={11}
              className={cn("transition-transform", open && "rotate-90")}
            />
            {summarise(catalog, bundle.kind)}
          </button>
          {open && catalog.length > 0 && (
            <ul className="mt-1.5 space-y-1">
              {catalog.map((entry) => (
                <li
                  key={`${entry.kind}:${entry.name}`}
                  className="flex items-baseline gap-2 text-[0.6875rem]"
                >
                  <code className="shrink-0 text-dim">
                    {sigilFor(entry.kind)}
                    {entry.name}
                  </code>
                  {entry.argumentHint && (
                    <span className="shrink-0 text-faint">
                      {entry.argumentHint}
                    </span>
                  )}
                  <span className="truncate text-faint">
                    {entry.description}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className="flex shrink-0 items-center gap-2">
          <Toggle
            label={t("skills.defaultForNew")}
            checked={bundle.enabledDefault}
            disabled={setDefault.isPending}
            onChange={() =>
              setDefault.mutate({
                name: bundle.name,
                enabledDefault: !bundle.enabledDefault,
              })
            }
          />
          {/* Only a clone has an upstream to re-read, and only a clone can be
              uninstalled: an authored bundle is changed by editing its skills,
              and its library row is a projection of them. The server refuses
              both — so offering either button would be offering an error. An
              authored plugin is deleted in the Authored section above, beside
              where it is written. */}
          {bundle.kind.kind !== "Authored" && (
            <>
              <button
                className="key shrink-0 key-sm"
                onClick={() => update.mutate(bundle.name)}
                disabled={update.isPending}
              >
                {update.isPending ? (
                  <Loader2 size={13} className="animate-spin" />
                ) : (
                  <RotateCcw size={13} />
                )}
                {t("skills.update")}
              </button>
              <button
                className="key-icon shrink-0 text-faint hover:text-red-ink"
                onClick={async () => {
                  if (
                    await askConfirm(
                      t("skills.confirmDeleteBundle", { name: bundle.name }),
                    )
                  )
                    remove.mutate(bundle.name);
                }}
                disabled={remove.isPending}
                aria-label={t("skills.deleteBundle")}
              >
                <Trash2 size={15} />
              </button>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function Toggle({
  label,
  checked,
  disabled,
  onChange,
}: {
  label: string;
  checked: boolean;
  disabled?: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={cn(
        "relative h-5 w-9 shrink-0 rounded-full transition-colors disabled:opacity-50",
        checked ? "bg-accent" : "bg-raised",
      )}
    >
      <span
        className={cn(
          "absolute top-0.5 h-4 w-4 rounded-full bg-white transition-transform",
          checked ? "translate-x-4" : "translate-x-0.5",
        )}
      />
    </button>
  );
}
