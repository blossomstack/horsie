import { Check, Pencil, Trash2, X } from "lucide-react";
import { useState } from "react";
import { getCurrentProject } from "../../api/client";
import { ReadError } from "../../components/ReadError";
import {
  useCreateProject,
  useDeleteProject,
  useProjects,
  useRenameProject,
} from "../../hooks/useProjects";
import { askConfirm } from "../../lib/confirm";
import { ListRow, RowAction, Section, SettingsPane } from "./fields";
import { SettingsHeader } from "./SettingsHeader";

/**
 * Settings → Projects.
 *
 * A project is the scope every other page on this rail configures: models,
 * runtimes, skills, memory and integrations all belong to one, and none of them
 * is visible from another. That is why this page says so out loud rather than
 * listing names silently — a second project starting empty is surprising until
 * you know it is the rule.
 */
export function ProjectsSettings() {
  const current = getCurrentProject();
  const projects = useProjects();
  const create = useCreateProject();
  const rename = useRenameProject();
  const remove = useDeleteProject();

  const [name, setName] = useState("");
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState("");

  const onDelete = async (id: string, label: string) => {
    const ok = await askConfirm(
      `Delete project “${label}”? Its sessions, agents, settings and memories go with it, and its runtimes are destroyed. This cannot be undone.`,
      "Delete",
    );
    if (!ok) return;
    await remove.mutateAsync(id);
    // Deleting the project you are standing in leaves the URL naming something
    // that no longer exists, and every query under it would 404. Leave first —
    // to `/`, which picks the default and is outside this router's basename.
    if (id === current) window.location.replace("/");
  };

  if (projects.error) {
    return (
      <SettingsPane>
        <SettingsHeader title="Projects" desc="This account's projects." />
        <ReadError error={projects.error} what="projects" />
      </SettingsPane>
    );
  }

  return (
    <SettingsPane>
      <SettingsHeader
        title="Projects"
        desc="One per body of work. Nothing is shared between them."
      />

      <Section
        title="Projects"
        desc="Everything else on this rail belongs to one project: its models, runtimes, skills, memory spaces, integrations, agents and sessions. A new project starts empty, credentials included."
        empty={
          projects.data && projects.data.length === 0
            ? "No projects yet."
            : null
        }
      >
        {projects.data?.map((p) => (
          <ListRow
            key={p.id}
            testId={`project-${p.id}`}
            title={
              editing === p.id ? (
                <input
                  className="field w-full"
                  data-testid="project-name"
                  value={draft}
                  autoFocus
                  onChange={(e) => setDraft(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Escape") setEditing(null);
                    if (e.key === "Enter") {
                      rename.mutate({ id: p.id, name: draft });
                      setEditing(null);
                    }
                  }}
                />
              ) : (
                p.name
              )
            }
            subtitle={p.id === current ? "Currently open" : undefined}
            meta={
              p.isDefault ? (
                <span className="chip" title="Always present, and cannot be deleted">
                  Default
                </span>
              ) : undefined
            }
            actions={
              editing === p.id ? (
                <>
                  <RowAction
                    icon={<Check size={13} aria-hidden />}
                    label="Save name"
                    onClick={() => {
                      rename.mutate({ id: p.id, name: draft });
                      setEditing(null);
                    }}
                  />
                  <RowAction
                    icon={<X size={13} aria-hidden />}
                    label="Cancel"
                    onClick={() => setEditing(null)}
                  />
                </>
              ) : (
                <>
                  <RowAction
                    icon={<Pencil size={13} aria-hidden />}
                    label="Rename"
                    testId={`rename-${p.id}`}
                    onClick={() => {
                      setEditing(p.id);
                      setDraft(p.name);
                    }}
                  />
                  <RowAction
                    icon={<Trash2 size={13} aria-hidden />}
                    label={
                      p.isDefault
                        ? "The default project cannot be deleted"
                        : "Delete"
                    }
                    testId={`delete-${p.id}`}
                    danger
                    disabled={p.isDefault}
                    onClick={() => void onDelete(p.id, p.name)}
                  />
                </>
              )
            }
          />
        ))}
      </Section>

      <Section title="New project" desc="It starts empty — add its models and runtimes once it exists.">
        <form
          className="flex gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (!name.trim()) return;
            create.mutate(name, { onSuccess: () => setName("") });
          }}
        >
          <input
            className="field flex-1"
            data-testid="new-project-name"
            placeholder="What is this project for?"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <button
            type="submit"
            className="key"
            data-testid="create-project"
            disabled={!name.trim() || create.isPending}
          >
            Create
          </button>
        </form>
      </Section>
    </SettingsPane>
  );
}
