import { useMemo, useState } from "react";
import type { AgentPresetInput, AgentView } from "../api/types";
import { useSettings } from "./useSettings";
import type { ConfigDraft } from "./useSessionDraft";

export interface AgentDraft extends ConfigDraft {
  /** Assemble the save payload. `name`/`description` come from the form's
   * text inputs, not the picker state. */
  buildAgentInput: (
    name: string,
    description: string,
    instructions: string,
  ) => AgentPresetInput;
}

/** Draft state for the agent-preset form. Unlike `useSessionDraft` nothing
 * persists to localStorage and there is no first-visit seeding — the preset
 * being edited (or empty defaults) is the source of truth.
 *
 * There is no environment here. A preset is agent configuration; where it runs
 * and what it runs against are supplied by whoever invokes it. */
export function useAgentDraft(initial?: AgentView): AgentDraft {
  const { data: settings } = useSettings();
  const [model, setModel] = useState(initial?.model ?? "");
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
  // No control renders for this any more, but `PUT` is a full replace: a
  // preset that an API caller turned compaction off on would silently have it
  // turned back on by anyone who opened the form and pressed Save. So it is
  // carried through untouched rather than dropped.
  const carriedAutoCompact = initial?.autoCompact;

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
      (
        name: string,
        description: string,
        instructions: string,
      ): AgentPresetInput => ({
        // `PUT` is a full replace, so anything omitted here is deleted.
        name: name.trim(),
        description: description.trim() || undefined,
        instructions: instructions.trim() || undefined,
        model: model.trim(),
        plugins: skills.size ? [...skills] : undefined,
        mcpServers: mcp.size ? [...mcp] : undefined,
        memorySpaces: memorySpaces.size ? [...memorySpaces] : undefined,
        thinkingEffort: effectiveThinkingEffort || undefined,
        autoCompact: carriedAutoCompact,
      }),
    [model, skills, mcp, memorySpaces, effectiveThinkingEffort, carriedAutoCompact],
  );

  return {
    model,
    setModel,
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
    buildAgentInput,
  };
}
