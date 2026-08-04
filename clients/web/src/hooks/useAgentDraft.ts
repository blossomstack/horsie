import { useMemo, useState } from "react";
import type { AgentPresetInput, AgentView, RepoConfig } from "../api/types";
import { useGithubStatus } from "./useGithub";
import { useSettings } from "./useSettings";
import type { ConfigDraft } from "./useSessionDraft";

export interface AgentDraft extends ConfigDraft {
  /** Assemble the save payload. `name`/`description` come from the form's
   * text inputs, not the picker state. */
  buildAgentInput: (name: string, description: string) => AgentPresetInput;
}

/** `https://github.com/org/repo` → `org/repo`; anything else is kept whole. */
function fullName(url: string): string {
  return url.replace(/^https:\/\/github\.com\//, "").replace(/\.git$/, "");
}

/** Draft state for the agent-preset form. Unlike `useSessionDraft` nothing
 * persists to localStorage and there is no first-visit seeding — the preset
 * being edited (or empty defaults) is the source of truth. */
export function useAgentDraft(initial?: AgentView): AgentDraft {
  const { data: settings } = useSettings();
  const { data: ghStatus } = useGithubStatus();
  const [model, setModel] = useState(initial?.model ?? "");
  const [repos, setRepos] = useState<Map<string, string>>(
    () =>
      new Map(
        (initial?.repos ?? []).map((r) => [fullName(r.url), r.gitRef ?? ""]),
      ),
  );
  const [skills, setSkills] = useState<Set<string>>(
    () => new Set(initial?.plugins ?? []),
  );
  const [mcp, setMcp] = useState<Set<string>>(
    () => new Set(initial?.mcpServers ?? []),
  );
  const [memorySpaces, setMemorySpaces] = useState<Set<string>>(
    () => new Set(initial?.memorySpaces ?? []),
  );
  const [thinkingEffort, setThinkingEffort] = useState(
    initial?.thinkingEffort ?? "",
  );

  // A preset names no vendor, so the only runtime it can be judged against is
  // the server default — the one it will actually be invoked on. This gates the
  // Repos picker and nothing else.
  const defaultVendor = (settings?.vendors ?? []).find(
    (v) => v.name === settings?.defaultVendor,
  );
  const provisions = !!defaultVendor?.capabilities?.supportsProvisioning;
  const githubConnected = !!ghStatus?.connected;

  // The effort menu belongs to the model, so a preset can name an effort the
  // currently-selected model no longer offers. Treat that as "use the model's
  // default" rather than saving a value the server would reject with a 422.
  const selectedModel = (settings?.models ?? []).find((m) => m.alias === model);
  const thinkingEfforts = selectedModel?.thinkingEfforts ?? [];
  const effectiveThinkingEffort = thinkingEfforts.includes(thinkingEffort)
    ? thinkingEffort
    : "";

  const buildAgentInput = useMemo(
    () =>
      (name: string, description: string): AgentPresetInput => {
        // `PUT` is a full replace, so anything omitted here is deleted. The
        // picker is hidden when the default vendor cannot provision, but the
        // preset's repos are still its repos — send them back rather than
        // wiping them because of a vendor the preset does not name.
        const repoList: RepoConfig[] = provisions
          ? [...repos.entries()].map(([fn, ref]) => ({
              url: `https://github.com/${fn}`,
              gitRef: ref.trim() || undefined,
            }))
          : (initial?.repos ?? []);
        return {
          name: name.trim(),
          description: description.trim() || undefined,
          model: model.trim(),
          repos: repoList.length ? repoList : undefined,
          plugins: skills.size ? [...skills] : undefined,
          mcpServers: mcp.size ? [...mcp] : undefined,
          memorySpaces: memorySpaces.size ? [...memorySpaces] : undefined,
          thinkingEffort: effectiveThinkingEffort || undefined,
        };
      },
    [
      provisions,
      repos,
      initial?.repos,
      model,
      skills,
      mcp,
      memorySpaces,
      effectiveThinkingEffort,
    ],
  );

  return {
    model,
    setModel,
    repos: new Map(repos),
    setRepos: (m) => setRepos(new Map(m)),
    skills: new Set(skills),
    setSkills: (s) => setSkills(new Set(s)),
    mcp: new Set(mcp),
    setMcp: (s) => setMcp(new Set(s)),
    memorySpaces: new Set(memorySpaces),
    setMemorySpaces: (s) => setMemorySpaces(new Set(s)),
    thinkingEffort: effectiveThinkingEffort,
    setThinkingEffort,
    thinkingEfforts,
    modelDefaultThinkingEffort: selectedModel?.thinkingEffort ?? "",
    provisions,
    githubConnected,
    buildAgentInput,
  };
}
