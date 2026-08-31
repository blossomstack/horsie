import { Pencil, Trash2 } from "lucide-react";
import { Link } from "react-router-dom";
import { useTranslation } from "react-i18next";
import type { EnvironmentView } from "../../api/types";
import { basename } from "../../lib/format";

/** A labelled block, so the four sections cannot drift apart. */
function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section>
      <h3 className="legend">{title}</h3>
      <div className="mt-2">{children}</div>
    </section>
  );
}

function None() {
  const { t } = useTranslation();
  return <p className="text-sm text-faint">{t("common.none")}</p>;
}

/**
 * One environment, read rather than edited.
 *
 * Everything a run inherits from it, in the order it happens: where it runs,
 * what is checked out, what is in the shell, and what is executed before the
 * agent gets a turn.
 */
export function EnvironmentDetail({
  environment,
  onDelete,
}: {
  environment: EnvironmentView;
  onDelete: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex h-full flex-col" data-testid="environment-detail">
      <header className="flex h-[var(--header-h)] shrink-0 items-center gap-2 bar-scroll px-6">
        <h2 className="page-title min-w-0 flex-1 truncate">{environment.name}</h2>
        <Link
          to={`/environments/${encodeURIComponent(environment.name)}/edit`}
          className="key key-sm"
          data-testid="edit-environment"
        >
          <Pencil size={14} aria-hidden />
          {t("common.edit")}
        </Link>
        <button
          className="key-icon hover:!bg-red-quiet hover:!text-red-ink"
          onClick={onDelete}
          title={t("common.deleteNamed", { name: environment.name })}
          aria-label={t("common.deleteNamed", { name: environment.name })}
          data-testid="delete-environment"
        >
          <Trash2 size={15} aria-hidden />
        </button>
      </header>

      <div className="flex-1 space-y-5 overflow-y-auto px-6 py-4">
        {environment.description && (
          <p className="max-w-prose text-sm text-dim">{environment.description}</p>
        )}

        <Section title={t("environmentEdit.vendor")}>
          <p className="font-mono text-sm text-legend">{environment.vendor}</p>
        </Section>

        <Section title={t("environment.repos")}>
          {environment.repos.length === 0 ? (
            <None />
          ) : (
            <ul className="space-y-1">
              {environment.repos.map((r) => (
                <li key={r.url} className="font-mono text-sm text-legend">
                  {basename(r.url)}
                  {r.gitRef && <span className="text-faint"> @ {r.gitRef}</span>}
                  {r.dir && <span className="text-faint"> → {r.dir}</span>}
                </li>
              ))}
            </ul>
          )}
        </Section>

        <Section title={t("environmentEdit.envVars")}>
          {environment.envVars.length === 0 ? (
            <None />
          ) : (
            <ul className="space-y-1">
              {environment.envVars.map((v) => (
                <li key={v.name} className="font-mono text-sm break-words text-legend">
                  {v.name}
                  <span className="text-faint">={v.value}</span>
                </li>
              ))}
            </ul>
          )}
        </Section>

        <Section title={t("environmentEdit.provision")}>
          {environment.provision.length === 0 ? (
            <None />
          ) : (
            <ol className="space-y-1">
              {environment.provision.map((s, i) => (
                <li key={`${s.name}-${i}`} className="text-sm text-legend">
                  <span className="font-mono">{s.name}</span>
                  <span className="text-faint"> · {s.uses}</span>
                </li>
              ))}
            </ol>
          )}
        </Section>
      </div>
    </div>
  );
}
