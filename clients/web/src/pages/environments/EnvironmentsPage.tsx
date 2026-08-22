import { useScrolledUnder } from "../../hooks/useScrolledUnder";
import { Plus } from "lucide-react";
import { RailToggle } from "../../components/rail";
import { RosterRow } from "../../components/RosterRow";
import { askConfirm } from "../../lib/confirm";
import { useNavigate } from "react-router-dom";
import { useEnvironments, useDeleteEnvironment } from "../../hooks/useEnvironments";

export function EnvironmentsPage() {
  const { onScroll, barProps } = useScrolledUnder();
  const { data: environments, isLoading, isError } = useEnvironments();
  const del = useDeleteEnvironment();
  const navigate = useNavigate();

  return (
    <div className="flex h-full flex-col" data-testid="environments-page">
      <div {...barProps}
        className="flex h-[var(--header-h)] shrink-0 items-center bar-scroll gap-2 bg-panel px-4 sm:gap-3 sm:px-6">
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
      <div className="flex-1 overflow-y-auto px-4 py-5 sm:px-6" onScroll={onScroll}>
          {isLoading && (
            <div className="flex items-center gap-2">
              <span className="lamp lamp-live text-live-ink" aria-hidden />
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
            <section className="section" data-testid="environments-empty">
              <h2 className="legend">Environment roster</h2>
              <p className="mt-3 max-w-prose text-sm leading-relaxed text-dim">
                An environment is a saved runtime + repos bundle — where the
                work runs and what is checked out there. Press{" "}
                <span className="text-legend">New environment</span> to define
                one.
              </p>
            </section>
          )}
          <div className="list-divided">
            {(environments ?? []).map((e) => (
              <RosterRow
                key={e.name}
                to={`/environments/${encodeURIComponent(e.name)}/edit`}
                name={e.name}
                meta={e.vendor}
                description={e.description}
                facts={
                  <>
                    {e.repos.length > 0 && (
                      <span className="legend">{e.repos.length} repos</span>
                    )}
                    {e.envVars.length > 0 && (
                      <span className="legend">{e.envVars.length} env</span>
                    )}
                    {e.provision.length > 0 && (
                      <span className="legend">{e.provision.length} steps</span>
                    )}
                  </>
                }
                testId="environment-row"
                nameAttr={{ "data-environment-name": e.name }}
                deleteLabel={`Delete ${e.name}`}
                deleteTestId={`delete-environment-${e.name}`}
                onDelete={async () => {
                  if (await askConfirm(`Delete environment '${e.name}'?`))
                    del.mutate(e.name);
                }}
              />
            ))}
          </div>
      </div>
    </div>
  );
}
