import { Trans, useTranslation } from "react-i18next";
import { Link, useLocation } from "react-router-dom";
import { RailToggle } from "../components/rail";

/**
 * The `path="*"` catch-all.
 *
 * Without it an unmatched route rendered a blank white page — zero DOM, not
 * even the rail, so the only escape was the URL bar. Reached by ordinary
 * typos: `/admin/github` for `/admin/github-app`, or any stale bookmark.
 *
 * It sits *inside* the sessions layout rather than standing alone, because the
 * navigation being gone was the actual defect; a full-page 404 with its own
 * chrome would fix the blankness and keep the dead end.
 */
export function NotFoundPage() {
  const { t } = useTranslation();
  const { pathname } = useLocation();

  return (
    <div className="flex h-full flex-col" data-testid="not-found-page">
      <div className="bar-scroll flex items-center gap-2 bg-panel px-4 py-3.5 sm:gap-3 sm:px-6">
        <RailToggle />
        <div className="min-w-0 flex-1">
          <h1 className="page-title">{t("notFound.title")}</h1>
          <p className="mt-0.5 text-xs text-faint">
{t("notFound.desc")}
          </p>
        </div>
      </div>
      <div className="flex-1 overflow-y-auto px-4 py-5 sm:px-6">
        <div className="mx-auto max-w-3xl">
          {/* Same shape as the empty roster on /agents: a labelled panel, not
            a centred icon-in-a-box. */}
          <section className="section">
            <h2 className="legend">{t("notFound.requestedPath")}</h2>
            {/* A path is a machine string, so it belongs in the mono face. */}
            <pre className="screen mt-3 overflow-x-auto px-3 py-2.5 font-mono text-[0.6875rem] leading-relaxed text-legend select-all">
              {pathname}
            </pre>
            <p className="mt-3 max-w-prose text-sm leading-relaxed text-dim">
              <Trans
                i18nKey="notFound.help"
                components={{
                  lnk: <Link className="text-legend underline" to="/" />,
                }}
              />
            </p>
          </section>
        </div>
      </div>
    </div>
  );
}
