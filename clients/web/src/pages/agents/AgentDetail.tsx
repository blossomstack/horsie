import { Pencil, Trash2 } from "lucide-react";
import { Link } from "react-router-dom";
import { useTranslation } from "react-i18next";
import type { AgentView } from "../../api/types";
import { ConfigFields } from "../../components/SessionConfigBar";
import { Prose } from "../../components/Prose";
import { useAgentDraft } from "../../hooks/useAgentDraft";

/**
 * One preset, read rather than edited.
 *
 * The configuration is the editor's own fields, frozen — `useAgentDraft` builds
 * the same draft the form builds, and `ConfigFields` renders it with the
 * controls off. A separate readout would be a second description of one thing,
 * which is the mistake the session's config row spent a whole change undoing.
 */
export function AgentDetail({
  agent,
  onDelete,
}: {
  agent: AgentView;
  onDelete: () => void;
}) {
  const { t } = useTranslation();
  const draft = useAgentDraft(agent);
  return (
    <div className="flex h-full flex-col" data-testid="agent-detail">
      <header className="flex h-[var(--header-h)] shrink-0 items-center gap-2 bar-scroll px-6">
        <h2 className="page-title min-w-0 flex-1 truncate">{agent.name}</h2>
        <Link
          to={`/agents/${encodeURIComponent(agent.name)}/edit`}
          className="key key-sm"
          data-testid="edit-agent"
        >
          <Pencil size={14} aria-hidden />
          {t("common.edit")}
        </Link>
        <button
          className="key-icon hover:!bg-red-quiet hover:!text-red-ink"
          onClick={onDelete}
          title={t("common.deleteNamed", { name: agent.name })}
          aria-label={t("common.deleteNamed", { name: agent.name })}
          data-testid="delete-agent"
        >
          <Trash2 size={15} aria-hidden />
        </button>
      </header>

      <div className="flex-1 space-y-5 overflow-y-auto px-6 py-4">
        {agent.description && (
          <p className="max-w-prose text-sm text-dim">{agent.description}</p>
        )}

        {/* What the model actually reads, which is the field that makes two
            presets on one model different agents. */}
        <section>
          <h3 className="legend">{t("agentEdit.instructions")}</h3>
          {agent.instructions ? (
            <div className="mt-2 max-w-prose">
              <Prose text={agent.instructions} />
            </div>
          ) : (
            <p className="mt-2 text-sm text-faint">{t("common.none")}</p>
          )}
        </section>

        <section>
          <h3 className="legend mb-2">{t("agentEdit.configuration")}</h3>
          <ConfigFields draft={draft} mode="frozen" />
        </section>

        <section>
          <h3 className="legend">{t("agentEdit.tuning")}</h3>
          <p className="mt-1 text-sm text-dim">
            {agent.tunable ? t("common.yes") : t("common.no")}
          </p>
        </section>
      </div>
    </div>
  );
}
