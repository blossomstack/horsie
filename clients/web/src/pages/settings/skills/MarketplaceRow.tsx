import {
  ChevronDown,
  ChevronRight,
  Download,
  Loader2,
  RotateCcw,
  Trash2,
} from "lucide-react";
import { useState } from "react";
import { askConfirm } from "../../../lib/confirm";
import type { MarketplaceView } from "../../../api/types";
import {
  useInstallPlugin,
  useRefreshMarketplace,
  useRemoveMarketplace,
} from "../../../hooks/usePlugins";
import { useTranslation } from "react-i18next";

/** One registered source: what it offers, and the two buttons that maintain it.
 *
 * The disclosure is controlled by the parent rather than held here, because
 * pasting a catalogue URL into the install box has to open the row it just
 * created — the outcome of an action at the top of the page is a row further
 * down it. */
export function MarketplaceRow({
  marketplace,
  expanded,
  onToggle,
}: {
  marketplace: MarketplaceView;
  expanded: boolean;
  onToggle: () => void;
}) {
  const install = useInstallPlugin();
  const { t } = useTranslation();
  const refresh = useRefreshMarketplace();
  const remove = useRemoveMarketplace();
  const [filter, setFilter] = useState("");

  const needle = filter.trim().toLowerCase();
  const shown = needle
    ? marketplace.plugins.filter(
        (p) =>
          p.name.toLowerCase().includes(needle) ||
          (p.description ?? "").toLowerCase().includes(needle),
      )
    : marketplace.plugins;

  return (
    <div
      className="rounded-[var(--radius-control)] "
      style={{ background: "var(--panel-raised)" }}
      data-testid="marketplace-row"
    >
      <div className="flex items-start gap-3 p-3">
        <button
          type="button"
          className="key-icon shrink-0 text-faint"
          onClick={onToggle}
          aria-expanded={expanded}
          aria-label={expanded ? "Hide plugins" : "Show plugins"}
        >
          {expanded ? <ChevronDown size={15} /> : <ChevronRight size={15} />}
        </button>

        <button
          type="button"
          className="min-w-0 flex-1 text-left"
          onClick={onToggle}
        >
          <div className="flex items-center gap-2">
            <span className="item-title truncate">{marketplace.name}</span>
            <span className="chip !py-0 text-[0.625rem]">
              {marketplace.pluginCount} plugin
              {marketplace.pluginCount === 1 ? "" : "s"}
            </span>
          </div>
          <p className="mt-0.5 truncate text-xs text-faint">
            {marketplace.sourceUrl}
          </p>
        </button>

        <div className="flex shrink-0 items-center gap-2">
          <button
            className="key shrink-0 key-sm"
            onClick={() => refresh.mutate(marketplace.name)}
            disabled={refresh.isPending}
          >
            {refresh.isPending ? (
              <Loader2 size={13} className="animate-spin" />
            ) : (
              <RotateCcw size={13} />
            )}
            Refresh
          </button>
          <button
            className="key-icon shrink-0 text-faint hover:text-red-ink"
            onClick={async () => {
              if (
                await askConfirm(
                  t("skills.confirmRemoveMarketplace", {
                    name: marketplace.name,
                  }),
                  t("common.remove"),
                )
              )
                remove.mutate(marketplace.name);
            }}
            disabled={remove.isPending}
            aria-label={t("skills.removeMarketplace")}
          >
            <Trash2 size={15} />
          </button>
        </div>
      </div>

      {expanded && (
        <div className="px-3 pb-3 pt-3">
          {/* The official catalogue lists ~276 plugins; without this the list is
              a scroll rather than a choice. */}
          <input
            className="field w-full"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder={t("skills.filterPlugins")}
            aria-label={t("skills.filterPluginsLabel")}
            data-testid="marketplace-filter"
          />

          <div className="mt-2.5 space-y-1.5">
            {shown.length === 0 && (
              <p className="screen px-3 py-3 text-center text-sm text-faint">
                Nothing matches “{filter.trim()}”.
              </p>
            )}
            {shown.map((p) => (
              <div
                key={p.name}
                className="flex items-start gap-3 rounded-[var(--radius-control)] px-3 py-2"
                data-testid="marketplace-entry"
              >
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="truncate text-sm">{p.name}</span>
                    {p.version && (
                      <span className="chip !py-0 text-[0.625rem]">
                        {p.version}
                      </span>
                    )}
                  </div>
                  {p.description && (
                    <p className="mt-0.5 text-xs text-dim">{p.description}</p>
                  )}
                </div>
                <button
                  className="key shrink-0 key-sm"
                  data-testid={`entry-install-${p.name}`}
                  disabled={p.installed || install.isPending}
                  onClick={() =>
                    install.mutate({
                      marketplace: marketplace.name,
                      pluginName: p.name,
                    })
                  }
                >
                  {install.isPending ? (
                    <Loader2 size={13} className="animate-spin" />
                  ) : (
                    <Download size={13} />
                  )}
                  {p.installed ? "Installed" : "Install"}
                </button>
              </div>
            ))}
          </div>

          {/* A catalogue that quietly lost three plugins is a bug report nobody
              files, so the entries this index could not parse are named. */}
          {marketplace.skipped.length > 0 && (
            <div
              className="mt-2.5 screen px-3 py-2 text-xs text-faint"
              data-testid="marketplace-skipped"
            >
              <p className="mb-1">
                {marketplace.skipped.length} entr
                {marketplace.skipped.length === 1 ? "y" : "ies"} could not be
                read:
              </p>
              <ul className="list-disc pl-4">
                {marketplace.skipped.map((why) => (
                  <li key={why}>{why}</li>
                ))}
              </ul>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
