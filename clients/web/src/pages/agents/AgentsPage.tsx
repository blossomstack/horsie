import { Bot, Plus, Trash2 } from "lucide-react";
import { Link, useNavigate } from "react-router-dom";
import { EmptyState } from "../../components/EmptyState";
import { useAgents, useDeleteAgent } from "../../hooks/useAgents";

export function AgentsPage() {
  const { data: agents, isLoading, isError } = useAgents();
  const del = useDeleteAgent();
  const navigate = useNavigate();

  return (
    <div className="flex h-full flex-col" data-testid="agents-page">
      <div className="flex items-center gap-3 border-b px-6 py-4">
        <h1 className="text-[15px] font-semibold text-text">Agents</h1>
        <button
          className="btn-primary ml-auto !px-2.5 !py-1.5 text-xs"
          onClick={() => navigate("/agents/new")}
          data-testid="new-agent-button"
        >
          <Plus size={15} />
          New agent
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-6 py-4">
        {isLoading && <p className="text-sm text-faint">Loading…</p>}
        {isError && (
          <p className="text-sm text-error">Can’t reach the server.</p>
        )}
        {agents && agents.length === 0 && (
          <EmptyState icon={<Bot size={24} />} title="No agents yet">
            An agent is a saved session setup — runtime, model, repos, skills,
            memory — that you invoke from the CLI with{" "}
            <code>horsie agent invoke &lt;name&gt; -m "…"</code>.
          </EmptyState>
        )}
        <div className="space-y-2">
          {(agents ?? []).map((a) => (
            <div
              key={a.name}
              className="flex items-center gap-3 rounded-[var(--radius)] border px-4 py-3"
              data-testid="agent-row"
              data-agent-name={a.name}
            >
              <Link
                to={`/agents/${encodeURIComponent(a.name)}/edit`}
                className="min-w-0 flex-1"
              >
                <div className="flex items-baseline gap-2">
                  <span className="font-mono text-sm font-medium text-text">
                    {a.name}
                  </span>
                  <span className="text-xs text-faint">
                    {a.model} · {a.vendor ?? "default runtime"}
                  </span>
                </div>
                {a.description && (
                  <div className="truncate text-sm text-muted">
                    {a.description}
                  </div>
                )}
                <div className="mt-1 flex gap-2 text-[11px] text-faint">
                  {a.plugins.length > 0 && (
                    <span>{a.plugins.length} skills</span>
                  )}
                  {a.memorySpaces.length > 0 && (
                    <span>{a.memorySpaces.length} memory</span>
                  )}
                  {a.mcpServers.length > 0 && (
                    <span>{a.mcpServers.length} MCP</span>
                  )}
                  {a.repos.length > 0 && <span>{a.repos.length} repos</span>}
                </div>
              </Link>
              <button
                className="rounded-[var(--radius-sm)] p-1.5 text-faint hover:bg-surface-2 hover:text-error"
                title={`Delete ${a.name}`}
                data-testid={`delete-agent-${a.name}`}
                onClick={() => {
                  if (window.confirm(`Delete agent '${a.name}'?`))
                    del.mutate(a.name);
                }}
              >
                <Trash2 size={15} />
              </button>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
