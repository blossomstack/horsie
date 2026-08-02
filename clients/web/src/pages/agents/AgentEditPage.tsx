import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { ApiRequestError } from "../../api/client";
import { SessionConfigBar } from "../../components/SessionConfigBar";
import type { AgentView } from "../../api/types";
import { useAgent, useCreateAgent, useUpdateAgent } from "../../hooks/useAgents";
import { useAgentDraft } from "../../hooks/useAgentDraft";

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
      <p className="px-6 py-4 text-sm text-error">No such agent: {name}.</p>
    );
  }
  return <AgentForm key={name ?? "new"} initial={existing} />;
}

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
  const canSave = !busy && agentName.trim() !== "" && draft.model.trim() !== "";

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
      <div className="border-b px-6 py-4">
        <h1 className="text-[15px] font-semibold text-text">
          {editing ? `Edit ${initial.name}` : "New agent"}
        </h1>
      </div>
      <div className="flex-1 overflow-y-auto px-6 py-4">
        <div className="mx-auto w-full max-w-3xl space-y-4">
          <label className="block">
            <span className="mb-1 block text-xs font-medium text-muted">
              Name
            </span>
            <input
              className="input w-full font-mono"
              placeholder="reviewer"
              value={agentName}
              disabled={editing}
              onChange={(e) => setAgentName(e.target.value)}
              data-testid="agent-name-input"
            />
          </label>
          <label className="block">
            <span className="mb-1 block text-xs font-medium text-muted">
              Description
            </span>
            <input
              className="input w-full"
              placeholder="What this agent is for"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              data-testid="agent-description-input"
            />
          </label>
          {error && (
            <div
              className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error"
              data-testid="agent-error"
            >
              {error}
            </div>
          )}
        </div>
      </div>
      <SessionConfigBar mode="draft" draft={draft} />
      <div className="mx-auto flex w-full max-w-3xl gap-2 px-4 pb-4">
        <button
          className="btn-primary"
          disabled={!canSave}
          onClick={handleSave}
          data-testid="save-agent-button"
        >
          {busy ? "Saving…" : "Save agent"}
        </button>
        <button className="btn-outline" onClick={() => navigate("/agents")}>
          Cancel
        </button>
      </div>
    </div>
  );
}
