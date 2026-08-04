import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { ApiRequestError } from "../../api/client";
import { ConfigFields } from "../../components/SessionConfigBar";
import { RailToggle } from "../../components/rail";
import type { AgentView } from "../../api/types";
import { useAgent, useCreateAgent, useUpdateAgent } from "../../hooks/useAgents";
import { useAgentDraft } from "../../hooks/useAgentDraft";
import { RowLabel } from "../settings/fields";

/** Create (`/agents/new`) and edit (`/agents/:name/edit`) share one form. The
 * form is a child component mounted only once the preset has loaded: its
 * pickers seed from `initial` with `useState`, which cannot pick up a value
 * that arrives later. */
export function AgentEditPage() {
  const { name } = useParams<{ name: string }>();
  const { data: existing, isLoading, isError } = useAgent(name);

  if (name && isLoading) {
    return <p className="px-6 py-4 text-sm text-faint">Loading…</p>;
  }
  if (name && (isError || !existing)) {
    return (
      <p className="px-6 py-4 text-sm text-red-ink">No such agent: {name}.</p>
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
  const [error, setError] = useState<string | null>(null);
  const draft = useAgentDraft(initial);
  const busy = create.isPending || update.isPending;
  // Name the requirement rather than just greying the button out: the Model
  // picker reads "Select" much like the optional Skills, MCP and Memory pickers
  // beside it, so a disabled Save with no message is a dead end.
  const blockedReason =
    agentName.trim() === ""
      ? "Give the agent a name to save it."
      : draft.model.trim() === ""
        ? "Pick a model to save this agent."
        : null;
  const canSave = !busy && blockedReason === null;

  const handleSave = async () => {
    setError(null);
    const body = draft.buildAgentInput(agentName, description);
    try {
      if (editing) await update.mutateAsync({ name: agentName.trim(), body });
      else await create.mutateAsync(body);
      navigate("/agents");
    } catch (e) {
      setError(
        e instanceof ApiRequestError ? e.message : "Failed to save agent.",
      );
    }
  };

  return (
    <div className="flex h-full flex-col" data-testid="agent-edit-page">
      <header className="flex h-[3.25rem] shrink-0 items-center gap-2 border-b bg-panel px-4 sm:px-6">
        <RailToggle />
        <h1 className="page-title min-w-0 flex-1 truncate">
          {editing ? `Edit ${initial.name}` : "New agent"}
        </h1>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto" data-popover-boundary>
        <div className="mx-auto w-full max-w-3xl space-y-6 px-4 py-6 sm:px-6">
          <section className="panel space-y-4 p-4">
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <label className="block">
                <RowLabel>Name</RowLabel>
                <input
                  className="field field-mono"
                  placeholder="reviewer"
                  value={agentName}
                  disabled={editing}
                  onChange={(e) => setAgentName(e.target.value)}
                  data-testid="agent-name-input"
                />
              </label>
              <label className="block">
                <RowLabel>Description</RowLabel>
                <input
                  className="field"
                  placeholder="What this agent is for"
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                  data-testid="agent-description-input"
                />
              </label>
            </div>

            <div className="border-t pt-4">
              <h2 className="section-title">Configuration</h2>
              <p className="mb-3 mt-1.5 max-w-prose text-xs leading-relaxed text-faint">
                What every session started from this preset runs with.
              </p>
              <ConfigFields draft={draft} />
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

          <div className="flex flex-wrap items-center gap-2">
            <button
              className="key key-go"
              disabled={!canSave}
              onClick={handleSave}
              data-testid="save-agent-button"
            >
              {busy ? "Saving…" : "Save agent"}
            </button>
            <button className="key key-blank" onClick={() => navigate("/agents")}>
              Cancel
            </button>
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
    </div>
  );
}
