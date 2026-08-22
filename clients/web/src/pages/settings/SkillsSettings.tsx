import { Boxes, Download, Loader2, Store } from "lucide-react";
import { useState } from "react";
import { ApiRequestError } from "../../api/client";
import {
  useInstallPlugin,
  useMarketplaces,
  usePlugins,
} from "../../hooks/usePlugins";
import { useAuthoredPlugins } from "../../hooks/useAuthored";
import { ReadError } from "../../components/ReadError";
import { TextField, SettingsPane } from "./fields";
import { SettingsHeader } from "./SettingsHeader";
import { AuthoredSection } from "./skills/AuthoredSection";
import { BundleRow } from "./skills/BundleRow";
import { MarketplaceRow } from "./skills/MarketplaceRow";

export function SkillsSettings() {
  const { data: bundles, isLoading, isError, error: bundlesError } = usePlugins();
  const {
    data: marketplaces,
    isError: marketplacesFailed,
    error: marketplacesError,
  } = useMarketplaces();
  const install = useInstallPlugin();

  const { data: authored } = useAuthoredPlugins();
  const [sourceUrl, setSourceUrl] = useState("");
  const [sourceRef, setSourceRef] = useState("");
  const [expanded, setExpanded] = useState<string | null>(null);

  const submitInstall = async () => {
    const url = sourceUrl.trim();
    if (!url) return;
    try {
      const outcome = await install.mutateAsync({
        sourceUrl: url,
        sourceRef: sourceRef.trim() || undefined,
      });
      setSourceUrl("");
      setSourceRef("");
      // A catalogue is not an error and not a dead end: its row appears below,
      // already open, so the next click is the one the person came to make.
      setExpanded(
        outcome.outcome === "Marketplace" ? outcome.value.name : null,
      );
    } catch {
      /* surfaced from install.error below */
    }
  };

  const installError =
    install.error instanceof ApiRequestError
      ? install.error.message
      : install.isError
        ? "Failed to install bundle."
        : null;

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <SettingsHeader
        title="Skills"
        desc="Shareable skill bundles installed from git repos — pick them per session."
      />

      <SettingsPane>
        <section className="section">
          <div className="mb-3 flex items-start gap-2">
            <Download size={15} className="mt-0.5 text-faint" />
            <div>
              <h2 className="section-title">Install a skill bundle</h2>
              <p className="mt-0.5 text-xs text-faint">
                A bundle, or a marketplace of them — horsie works out which.
                This can take a few seconds.
              </p>
            </div>
          </div>

          <div className="grid grid-cols-[1fr_auto] gap-3">
            <TextField
              label="Git URL"
              value={sourceUrl}
              onChange={setSourceUrl}
              placeholder="https://github.com/owner/skills-bundle"
            />
            <TextField
              label="Ref (optional)"
              value={sourceRef}
              onChange={setSourceRef}
              placeholder="main"
            />
          </div>

          {installError && (
            <div className="mt-3 rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
              {installError}
            </div>
          )}

          <div className="mt-3 flex justify-end">
            <button
              className="key key-go"
              onClick={submitInstall}
              disabled={!sourceUrl.trim() || install.isPending}
            >
              {install.isPending ? (
                <Loader2 size={15} className="animate-spin" />
              ) : (
                <Download size={15} />
              )}
              Install
            </button>
          </div>
        </section>

        {/* Hidden entirely until there is a catalogue to show, so a server with
            none looks exactly as it did before marketplaces existed — but a
            *failed* read is not "none", and silently removing the section is
            how a catalogue someone added appears to have been deleted. */}
        {(marketplacesFailed || (marketplaces && marketplaces.length > 0)) && (
          <section className="section">
            <div className="mb-3 flex items-start gap-2">
              <Store size={15} className="mt-0.5 text-faint" />
              <div>
                <h2 className="section-title">Marketplaces</h2>
                <p className="mt-0.5 text-xs text-faint">
                  Catalogues you have added. Removing one leaves its installed
                  bundles in place.
                </p>
              </div>
            </div>

            <div className="space-y-2.5">
              {marketplacesFailed && (
                <ReadError
                  what="marketplaces"
                  error={marketplacesError}
                  testId="marketplaces-error"
                />
              )}
              {(marketplaces ?? []).map((m) => (
                <MarketplaceRow
                  key={m.name}
                  marketplace={m}
                  expanded={expanded === m.name}
                  onToggle={() =>
                    setExpanded(expanded === m.name ? null : m.name)
                  }
                />
              ))}
            </div>
          </section>
        )}

        <AuthoredSection plugins={authored ?? []} />

        <section className="section">
          <div className="mb-3 flex items-start gap-2">
            <Boxes size={15} className="mt-0.5 text-faint" />
            <div>
              <h2 className="section-title">Installed bundles</h2>
              <p className="mt-0.5 text-xs text-faint">
                Toggle a bundle on to pre-select it for new sessions.
              </p>
            </div>
          </div>

          <div className="space-y-2.5">
            {isLoading && (
              <p className="py-8 text-center text-sm text-faint">Loading…</p>
            )}
            {isError && (
              <ReadError
                what="skill bundles"
                error={bundlesError}
                testId="bundles-error"
              />
            )}
            {bundles && bundles.length === 0 && (
              <p className="screen px-3 py-4 text-center text-sm text-faint">
                No skill bundles installed yet.
              </p>
            )}
            {bundles?.map((b) => (
              <BundleRow key={b.name} bundle={b} />
            ))}
          </div>
        </section>
      </SettingsPane>
    </div>
  );
}
