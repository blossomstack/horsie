import { Plus, Trash2 } from "lucide-react";
import { RailToggle } from "../../components/rail";
import { askConfirm } from "../../lib/confirm";
import { Link, useNavigate } from "react-router-dom";
import { useEnvironments, useDeleteEnvironment } from "../../hooks/useEnvironments";

export function EnvironmentsPage() {
  const { data: environments, isLoading, isError } = useEnvironments();
  const del = useDeleteEnvironment();
  const navigate = useNavigate();

  return (
    <div className="flex h-full flex-col" data-testid="environments-page">
      <div className="flex h-[3.25rem] shrink-0 items-center gap-2 border-b bg-panel px-4 sm:gap-3 sm:px-6">
        <RailToggle />
        <h1 className="page-title min-w-0 flex-1 truncate">Environments</h1>
        <button
          className="key key-go shrink-0"
          onClick={() => navigate("/environments/new")}
          data-testid="new-environment-button"
        >
          <Plus size={13} aria-hidden />
          New environment
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-4 py-5 sm:px-6">
        <div className="mx-auto max-w-3xl">
          {isLoading && (
            <div className="flex items-center gap-2">
              <span className="lamp lamp-live text-amber-ink" aria-hidden />
              <span className="legend">Loading environments</span>
            </div>
          )}
          {isError && (
            <p className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
              Can’t reach the server. Check that horsie-server is running, then
              reload.
            </p>
          )}
          {environments && environments.length === 0 && (
            <section className="panel p-4" data-testid="environments-empty">
              <h2 className="legend">Environment roster</h2>
              <p className="mt-3 max-w-prose text-sm leading-relaxed text-dim">
                An environment is a saved runtime + repos bundle — where the
                work runs and what is checked out there. Press{" "}
                <span className="text-legend">New environment</span> to define
                one.
              </p>
            </section>
          )}
          <div className="space-y-2">
            {(environments ?? []).map((e) => (
              <div
                key={e.name}
                className="flex items-center gap-3 rounded-[var(--radius-control)] border bg-panel px-4 py-3 transition-colors hover:bg-raised"
                data-testid="environment-row"
                data-environment-name={e.name}
              >
                <Link
                  to={`/environments/${encodeURIComponent(e.name)}/edit`}
                  className="min-w-0 flex-1"
                >
                  <div className="flex items-baseline gap-2">
                    <span className="font-mono text-sm font-medium text-legend">
                      {e.name}
                    </span>
                    <span className="legend">{e.vendor}</span>
                  </div>
                  {e.description && (
                    <div className="truncate text-sm text-dim">
                      {e.description}
                    </div>
                  )}
                  <div className="mt-1.5 flex flex-wrap gap-x-3 gap-y-1">
                    {e.repos.length > 0 && (
                      <span className="legend">{e.repos.length} repos</span>
                    )}
                    {e.envVars.length > 0 && (
                      <span className="legend">{e.envVars.length} env</span>
                    )}
                    {e.provision.length > 0 && (
                      <span className="legend">{e.provision.length} steps</span>
                    )}
                  </div>
                </Link>
                <button
                  className="key-icon shrink-0 !h-7 !w-7 hover:!bg-red-quiet hover:!text-red-ink"
                  title={`Delete ${e.name}`}
                  data-testid={`delete-environment-${e.name}`}
                  onClick={async () => {
                    if (await askConfirm(`Delete environment '${e.name}'?`))
                      del.mutate(e.name);
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
