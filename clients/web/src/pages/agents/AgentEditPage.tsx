import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { ApiRequestError } from "../../api/client";
import { ConfigFields } from "../../components/SessionConfigBar";
import type { AgentView } from "../../api/types";
import { useAgent, useCreateAgent, useUpdateAgent } from "../../hooks/useAgents";
import { useAgentDraft } from "../../hooks/useAgentDraft";
import { RowLabel } from "../settings/fields";
import { useTranslation } from "react-i18next";

/** Create (`/agents/new`) and edit (`/agents/:name/edit`) share one form. The
 * form is a child component mounted only once the preset has loaded: its
 * pickers seed from `initial` with `useState`, which cannot pick up a value
 * that arrives later. */
export function AgentEditPage() {
  const { t } = useTranslation();
  const { name } = useParams<{ name: string }>();
  const { data: existing, isLoading, isError } = useAgent(name);

  if (name && isLoading) {
    return <p className="px-6 py-4 text-sm text-faint">{t("common.loading")}</p>;
  }
  if (name && (isError || !existing)) {
    return (
      <p className="px-6 py-4 text-sm text-red-ink">
        {t("agentEdit.noSuch", { name })}
      </p>
    );
  }
  return <AgentForm key={name ?? "new"} initial={existing} />;
}

/**
 * One panel, read top to bottom: what this agent is called, what it is for,
 * and how it runs.
 *
 * The configuration used to be the session action row, rendered verbatim and
 * pinned to the bottom of the pane — so a preset's model sat below the save
 * button, separated from the name and description by the whole height of the
 * page. They are one form.
 *
 * The channels here are the session's minus the runtime: a preset does not name
 * one, because where the work runs belongs to the invocation.
 */
function AgentForm({ initial }: { initial?: AgentView }) {
  const editing = !!initial;
  const create = useCreateAgent();
  const update = useUpdateAgent();
  const navigate = useNavigate();
  const [agentName, setAgentName] = useState(initial?.name ?? "");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [instructions, setInstructions] = useState(initial?.instructions ?? "");
  const [error, setError] = useState<string | null>(null);
  const draft = useAgentDraft(initial);
  const { t } = useTranslation();
  // Both ways out of the form land on the panel the preset was being read in.
  // Returning to the bare roster threw away the selection the person had just
  // made, so saving an edit and then looking at what you saved was two clicks.
  const back = () =>
    navigate(initial ? `/agents/${encodeURIComponent(initial.name)}` : "/agents");
  const busy = create.isPending || update.isPending;
  // Name the requirement rather than just greying the button out: the Model
  // picker reads "Select" much like the optional Skills, MCP and Memory pickers
  // beside it, so a disabled Save with no message is a dead end.
  const blockedReason =
    agentName.trim() === ""
      ? t("agentEdit.needName")
      : draft.model.trim() === ""
        ? t("agentEdit.needModel")
        : null;
  const canSave = !busy && blockedReason === null;

  const handleSave = async () => {
    setError(null);
    const body = draft.buildAgentInput(agentName, description, instructions);
    try {
      if (editing) await update.mutateAsync({ name: agentName.trim(), body });
      else await create.mutateAsync(body);
      navigate(`/agents/${encodeURIComponent(agentName.trim())}`);
    } catch (e) {
      setError(
        e instanceof ApiRequestError ? e.message : t("agentEdit.saveFailed"),
      );
    }
  };

  return (
    <div className="flex h-full flex-col" data-testid="agent-edit-page">
      <header className="flex h-[var(--header-h)] shrink-0 items-center gap-2 bar-scroll px-6">
        <h1 className="page-title min-w-0 flex-1 truncate">
          {editing ? t("agentEdit.editTitle", { name: initial.name }) : t("agents.new")}
        </h1>
        <button
          className="key key-blank key-sm"
          onClick={back}
          data-testid="cancel-agent-button"
        >
          {t("common.cancel")}
        </button>
        <button
          className="key key-go key-sm"
          disabled={!canSave}
          onClick={handleSave}
          data-testid="save-agent-button"
        >
          {busy ? t("common.saving") : t("common.save")}
        </button>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto" data-popover-boundary>
        <div className="w-full space-y-6 px-6 py-4">
          <section className="section space-y-4">
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <label className="block">
                <RowLabel>{t("memoryPage.name")}</RowLabel>
                <input
                  className="field field-mono"
                  placeholder={t("agentEdit.namePlaceholder")}
                  value={agentName}
                  disabled={editing}
                  onChange={(e) => setAgentName(e.target.value)}
                  data-testid="agent-name-input"
                />
              </label>
              <label className="block">
                <RowLabel>{t("memoryPage.description")}</RowLabel>
                <input
                  className="field"
                  placeholder={t("agentEdit.descriptionPlaceholder")}
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                  data-testid="agent-description-input"
                />
                <p className="mt-1 text-xs text-faint">
{t("agentEdit.descriptionHint")}
                </p>
              </label>
              {/* The full width of the grid: it is a textarea sharing a row
                  with two single-line inputs, and at one column it rendered the
                  width of the Name box — the smallest control on the form for
                  the longest thing anyone types into it. */}
              <label className="block sm:col-span-2">
                <RowLabel>{t("agentEdit.instructions")}</RowLabel>
                {/* A textarea, and its own field: the description sat directly
                    above the configuration and read like the place to say how
                    the agent should behave, while being the one field that
                    never reached the model. */}
                <textarea
                  className="field min-h-28 resize-y"
                  placeholder={t("agentEdit.instructionsPlaceholder")}
                  value={instructions}
                  maxLength={8000}
                  onChange={(e) => setInstructions(e.target.value)}
                  data-testid="agent-instructions-input"
                />
                <p className="mt-1 text-xs text-faint">
{t("agentEdit.instructionsHint")}
                </p>
              </label>
            </div>

            <div className="pt-4">
              <h2 className="section-title">{t("agentEdit.configuration")}</h2>
              <p className="mb-3 mt-1.5 max-w-prose text-xs leading-relaxed text-faint">
{t("agentEdit.configurationHint")}
              </p>
              <ConfigFields draft={draft} />
              {/* "Let this agent manage this horsie server" was a checkbox
                  here. It is now the Horsie group in the Tools picker above:
                  the grant was always a question about which tools the agent
                  gets, and a separate bit beside the list could disagree with
                  it. Picking the tools is the grant. */}
            </div>

            {/* Its own block below the configuration, not a field inside it:
                everything above says how this agent runs, and this says who
                else may change that. */}
            <div className="pt-4">
              <h2 className="section-title">{t("agentEdit.tuning")}</h2>
              <label className="mt-2 flex items-start gap-2 text-sm">
                <input
                  type="checkbox"
                  className="mt-0.5"
                  checked={draft.tunable}
                  onChange={(e) => draft.setTunable(e.target.checked)}
                  data-testid="agent-tunable"
                />
                <span>
                  {t("agentEdit.tunable")}
                  <span className="mt-1 block max-w-prose text-xs leading-relaxed text-faint">
                    {t("agentEdit.tunableHint")}
                  </span>
                </span>
              </label>
            </div>
          </section>

          {error && (
            <div
              className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink"
              data-testid="agent-error"
            >
              {error}
            </div>
          )}

          {blockedReason && (
            <p
              className="text-xs leading-relaxed text-dim"
              data-testid="agent-blocked-hint"
            >
              {blockedReason}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}
