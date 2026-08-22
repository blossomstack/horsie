import { Plus } from "lucide-react";
import { RailToggle } from "../../components/rail";
import { RosterRow } from "../../components/RosterRow";
import { askConfirm } from "../../lib/confirm";
import { useNavigate } from "react-router-dom";
import { useAgents, useDeleteAgent } from "../../hooks/useAgents";

export function AgentsPage() {
  const { data: agents, isLoading, isError } = useAgents();
  const del = useDeleteAgent();
  const navigate = useNavigate();

  return (
    <div className="flex h-full flex-col" data-testid="agents-page">
      <div className="flex h-[var(--header-h)] shrink-0 items-center bar-edge-b gap-2 bg-panel px-4 sm:gap-3 sm:px-6">
        <RailToggle />
        <h1 className="page-title min-w-0 flex-1 truncate">Agents</h1>
        <button
          className="key key-go shrink-0"
          onClick={() => navigate("/agents/new")}
          data-testid="new-agent-button"
        >
          <Plus size={13} aria-hidden />
          New agent
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-4 py-4 sm:px-6">
        <div className="mx-auto max-w-3xl">
          {isLoading && (
            <div className="flex items-center gap-2">
              <span className="lamp lamp-live text-live-ink" aria-hidden />
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
            <section className="section" data-testid="agents-empty">
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
          <div className="list-divided">
            {(agents ?? []).map((a) => (
              <RosterRow
                key={a.name}
                to={`/agents/${encodeURIComponent(a.name)}/edit`}
                name={a.name}
                meta={a.model}
                description={a.description}
                facts={
                  <>
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
                  </>
                }
                testId="agent-row"
                nameAttr={{ "data-agent-name": a.name }}
                deleteLabel={`Delete ${a.name}`}
                deleteTestId={`delete-agent-${a.name}`}
                onDelete={async () => {
                  if (await askConfirm(`Delete agent '${a.name}'?`))
                    del.mutate(a.name);
                }}
              />
            ))}
          </div>
        </div>
      </div>
    </div>
  );
}
