import { Plus, Trash2 } from "lucide-react";
import { RailToggle } from "../../components/rail";
import { Link, useNavigate } from "react-router-dom";
import { useAgents, useDeleteAgent } from "../../hooks/useAgents";

export function AgentsPage() {
  const { data: agents, isLoading, isError } = useAgents();
  const del = useDeleteAgent();
  const navigate = useNavigate();

  return (
    <div className="flex h-full flex-col" data-testid="agents-page">
      <div className="flex items-center gap-2 border-b bg-panel px-4 py-3.5 sm:gap-3 sm:px-6">
        <RailToggle />
        <div className="min-w-0 flex-1">
          <h1 className="page-title">
            Agents
          </h1>
          <p className="mt-0.5 text-xs text-faint">
            Saved session setups you invoke from the CLI.
          </p>
        </div>
        <button
          className="key key-go shrink-0"
          onClick={() => navigate("/agents/new")}
          data-testid="new-agent-button"
        >
          <Plus size={13} aria-hidden />
          New agent
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-4 py-5 sm:px-6">
        <div className="mx-auto max-w-3xl">
          {isLoading && (
            <div className="flex items-center gap-2">
              <span className="lamp lamp-live text-amber-ink" aria-hidden />
              <span className="legend">Loading agents</span>
            </div>
          )}
          {isError && (
            <p className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
              Can’t reach the server. Check that horsie-server is running, then
              reload.
            </p>
          )}
          {/* Not a centred icon-in-a-box: an empty roster is a labelled blank
            slot on the panel, and the label is the command that fills it. */}
          {agents && agents.length === 0 && (
            <section className="panel p-4" data-testid="agents-empty">
              <h2 className="legend">Agent roster</h2>
              <p className="mt-3 max-w-prose text-sm leading-relaxed text-dim">
                An agent is a saved session setup — runtime, model, repos,
                skills, memory — so a run you repeat does not have to be
                reassembled each time. Press{" "}
                <span className="text-legend">New agent</span> to define one,
                then invoke it from any machine:
              </p>
              <pre className="screen mt-3 overflow-x-auto px-3 py-2.5 font-mono text-[0.6875rem] leading-relaxed text-legend select-all">
                horsie agent invoke &lt;name&gt; -m "…"
              </pre>
            </section>
          )}
          <div className="space-y-2">
            {(agents ?? []).map((a) => (
              <div
                key={a.name}
                className="flex items-center gap-3 rounded-[var(--radius-control)] border bg-panel px-4 py-3 transition-colors hover:bg-raised"
                data-testid="agent-row"
                data-agent-name={a.name}
              >
                <Link
                  to={`/agents/${encodeURIComponent(a.name)}/edit`}
                  className="min-w-0 flex-1"
                >
                  <div className="flex items-baseline gap-2">
                    <span className="font-mono text-sm font-medium text-legend">
                      {a.name}
                    </span>
                    <span className="legend">
                      {a.model}
                    </span>
                  </div>
                  {a.description && (
                    <div className="truncate text-sm text-dim">
                      {a.description}
                    </div>
                  )}
                  <div className="mt-1.5 flex flex-wrap gap-x-3 gap-y-1">
                    {a.plugins.length > 0 && (
                      <span className="legend">{a.plugins.length} skills</span>
                    )}
                    {a.memorySpaces.length > 0 && (
                      <span className="legend">
                        {a.memorySpaces.length} memory
                      </span>
                    )}
                    {a.mcpServers.length > 0 && (
                      <span className="legend">{a.mcpServers.length} MCP</span>
                    )}
                  </div>
                </Link>
                <button
                  className="key-icon shrink-0 !h-7 !w-7 hover:!bg-red-quiet hover:!text-red-ink"
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
    </div>
  );
}
