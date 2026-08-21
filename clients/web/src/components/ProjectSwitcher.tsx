import { Check, FolderTree, Plus } from "lucide-react";
import { Link } from "react-router-dom";
import { getCurrentProject } from "../api/client";
import { useProjects } from "../hooks/useProjects";
import { projectHome } from "../pages/ProjectScope";
import { PopoverMenu } from "./PopoverMenu";

/**
 * Which project the rail below belongs to, and how to leave it.
 *
 * At the top of the rail rather than buried in settings because everything
 * under it — the agents, the environments, the sessions — is scoped to this one
 * answer, and a person who does not know which project they are in cannot read
 * the rest of the column. It sits *outside* the nameplate for the same reason
 * the offline lamp sits inside it: the lamp is live state, this is where you
 * are.
 *
 * Switching is a full page load, not a route change. That is deliberate: no
 * query cache, no component state and no in-flight request survives, so nothing
 * from the project just left can be painted under the name of the one arrived
 * at.
 */
export function ProjectSwitcher() {
  const current = getCurrentProject();
  const { data } = useProjects();
  const here = data?.find((p) => p.id === current);

  return (
    <div className="px-2 pt-2">
      <PopoverMenu
        label={here?.name ?? "…"}
        legend="Project"
        placement="down"
        className="w-full"
        testId="project-switcher"
      >
        {(close) => (
          <div className="space-y-px">
            {data?.map((p) => (
              <a
                key={p.id}
                href={projectHome(p.id)}
                data-popover-option
                data-testid={`switch-to-${p.id}`}
                className="flex w-full items-center gap-2 rounded-[var(--radius-control)] px-2 py-1.5 text-left text-sm hover:bg-raised"
                onClick={close}
              >
                <FolderTree size={14} aria-hidden className="shrink-0 text-faint" />
                <span className="min-w-0 flex-1 truncate">{p.name}</span>
                {p.id === current && (
                  <Check size={14} aria-hidden className="shrink-0 text-legend" />
                )}
              </a>
            ))}
            <Link
              to="/settings/projects"
              data-popover-option
              data-testid="manage-projects"
              className="mt-1 flex w-full items-center gap-2 border-t px-2 pb-1.5 pt-2 text-left text-sm hover:bg-raised"
              onClick={close}
            >
              <Plus size={14} aria-hidden className="shrink-0 text-faint" />
              <span>New project…</span>
            </Link>
          </div>
        )}
      </PopoverMenu>
    </div>
  );
}
