import { Plus } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";
import { useNavigate, useParams } from "react-router-dom";
import { ListDetail, NothingSelected } from "../../components/ListDetail";
import { RosterRow } from "../../components/RosterRow";
import { askConfirm } from "../../lib/confirm";
import { useAgents, useDeleteAgent } from "../../hooks/useAgents";
import { AgentDetail } from "./AgentDetail";
import { AgentEditPage } from "./AgentEditPage";

/**
 * The roster, and beside it whichever agent is selected — read, or being
 * edited. `editing` is set by the `new` and `:name/edit` routes: the form is
 * the same width as the readout it replaces, so choosing another preset is
 * still one click away while you fill it in.
 */
export function AgentsPage({ editing }: { editing?: boolean }) {
  const { t } = useTranslation();
  const { name } = useParams<{ name: string }>();
  const { data: agents, isLoading, isError } = useAgents();
  const del = useDeleteAgent();
  const navigate = useNavigate();

  const selected = agents?.find((a) => a.name === name);

  const remove = async (agent: string) => {
    if (!(await askConfirm(t("agents.confirmDelete", { name: agent })))) return;
    del.mutate(agent);
    // Only when the thing being read is the thing being deleted: deleting a
    // row further down the roster must not empty the panel you were reading.
    if (agent === name) navigate("/agents");
  };

  return (
    <ListDetail
      testId="agents-page"
      title={t("nav.agents")}
      action={
        <button
          className="key key-go key-sm shrink-0"
          onClick={() => navigate("/agents/new")}
          data-testid="new-agent-button"
        >
          <Plus size={13} aria-hidden />
          {t("agents.new")}
        </button>
      }
      detail={
        editing ? (
          <AgentEditPage />
        ) : selected ? (
          <AgentDetail agent={selected} onDelete={() => void remove(selected.name)} />
        ) : (
          <NothingSelected>{t("agents.pickOne")}</NothingSelected>
        )
      }
    >
      {isLoading && (
        <div className="flex items-center gap-2 px-2.5 py-6">
          <span className="lamp lamp-live text-live-ink" aria-hidden />
          <span className="legend">{t("agents.loading")}</span>
        </div>
      )}
      {isError && (
        <p className="px-2.5 py-6 text-sm text-red-ink">{t("rail.unreachable")}</p>
      )}
      {/* Not a centred icon-in-a-box: an empty roster is a labelled blank slot
          on the panel, and the label is the command that fills it. */}
      {agents && agents.length === 0 && (
        <section className="section m-1" data-testid="agents-empty">
          <h2 className="legend">{t("agents.rosterTitle")}</h2>
          <p className="mt-3 text-sm leading-relaxed text-dim">
            <Trans
              i18nKey="agents.rosterBlurb"
              components={{ key: <span className="text-legend" /> }}
            />
          </p>
        </section>
      )}
      <div className="list-divided">
        {(agents ?? []).map((a) => (
          <RosterRow
            key={a.name}
            to={`/agents/${encodeURIComponent(a.name)}`}
            name={a.name}
            description={a.description}
            selected={a.name === name}
            testId="agent-row"
            nameAttr={{ "data-agent-name": a.name }}
            deleteLabel={t("common.deleteNamed", { name: a.name })}
            deleteTestId={`delete-agent-${a.name}`}
            onDelete={() => void remove(a.name)}
          />
        ))}
      </div>
    </ListDetail>
  );
}
