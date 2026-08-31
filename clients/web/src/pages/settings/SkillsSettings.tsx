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
import { TextField, SettingsPage } from "./fields";
import { AuthoredSection } from "./skills/AuthoredSection";
import { BundleRow } from "./skills/BundleRow";
import { MarketplaceRow } from "./skills/MarketplaceRow";
import { useTranslation } from "react-i18next";

export function SkillsSettings() {
  const { t } = useTranslation();
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
    <SettingsPage
        title={t("settingsNav.skills")}
    >
        <section className="section">
          <div className="mb-3 flex items-start gap-2">
            <Download size={15} className="mt-0.5 text-faint" />
            <div>
              <h2 className="section-title">{t("skillsPage.installTitle")}</h2>
            </div>
          </div>

          <div className="grid grid-cols-[1fr_auto] gap-3">
            <TextField
              label={t("skillsPage.gitUrl")}
              value={sourceUrl}
              onChange={setSourceUrl}
              placeholder={t("skillsPage.gitUrlPlaceholder")}
            />
            <TextField
              label={t("skillsPage.ref")}
              value={sourceRef}
              onChange={setSourceRef}
              placeholder={t("skillsPage.refPlaceholder")}
            />
          </div>

          {installError && (
            <div className="notice notice-fault mt-3">
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
              {t("skillsPage.install")}
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
                <h2 className="section-title">{t("skillsPage.marketplaces")}</h2>
              </div>
            </div>

            <div className="space-y-2.5">
              {marketplacesFailed && (
                <ReadError
                  what={t("skillsPage.marketplacesWhat")}
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
              <h2 className="section-title">{t("skillsPage.installedTitle")}</h2>
              <p className="mt-0.5 text-xs text-faint">
{t("skillsPage.installedDesc")}
              </p>
            </div>
          </div>

          <div className="space-y-2.5">
            {isLoading && (
              <p className="py-8 text-center text-sm text-faint">{t("common.loading")}</p>
            )}
            {isError && (
              <ReadError
                what={t("channel.skillBundles")}
                error={bundlesError}
                testId="bundles-error"
              />
            )}
            {bundles && bundles.length === 0 && (
              <p className="screen px-3 py-4 text-center text-sm text-faint">
{t("skillsPage.empty")}
              </p>
            )}
            {bundles?.map((b) => (
              <BundleRow key={b.name} bundle={b} />
            ))}
          </div>
        </section>
      </SettingsPage>
  );
}
