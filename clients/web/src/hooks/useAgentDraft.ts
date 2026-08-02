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
  const [vendor, setVendor] = useState(initial?.vendor ?? "");
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

  const activeVendors = settings?.vendors ?? [];
  const selectedVendor = activeVendors.find(
    (v) => v.name === (vendor || settings?.defaultVendor),
  );
  const provisions = !!selectedVendor?.capabilities?.supportsProvisioning;
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
        const repoList: RepoConfig[] = provisions
          ? [...repos.entries()].map(([fn, ref]) => ({
              url: `https://github.com/${fn}`,
              gitRef: ref.trim() || undefined,
            }))
          : [];
        return {
          name: name.trim(),
          description: description.trim() || undefined,
          vendor: vendor.trim() || undefined,
          model: model.trim(),
          repos: repoList.length ? repoList : undefined,
          plugins: provisions && skills.size ? [...skills] : undefined,
          mcpServers: provisions && mcp.size ? [...mcp] : undefined,
          // Not gated on `provisions`: memories are served by the server
          // itself, so they work on every vendor.
          memorySpaces: memorySpaces.size ? [...memorySpaces] : undefined,
          thinkingEffort: effectiveThinkingEffort || undefined,
        };
      },
    [
      provisions,
      repos,
      vendor,
      model,
      skills,
      mcp,
      memorySpaces,
      effectiveThinkingEffort,
    ],
  );

  return {
    vendor,
    setVendor,
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
