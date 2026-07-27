import { useEffect, useMemo, useState } from "react";
import type { CreateSessionRequest, RepoConfig } from "../api/types";
import { useGithubStatus } from "./useGithub";
import { usePlugins } from "./usePlugins";
import { useSettings } from "./useSettings";

export interface SessionDraft {
  vendor: string;
  setVendor: (v: string) => void;
  model: string;
  setModel: (m: string) => void;
  /** fullName → gitRef ("" = default branch). */
  repos: Map<string, string>;
  setRepos: (m: Map<string, string>) => void;
  skills: Set<string>;
  setSkills: (s: Set<string>) => void;
  mcp: Set<string>;
  setMcp: (s: Set<string>) => void;
  /** Memory spaces the session may read and write. */
  memorySpaces: Set<string>;
  setMemorySpaces: (s: Set<string>) => void;
  provisions: boolean;
  githubConnected: boolean;
  canSend: boolean;
  blockedReason: string | null;
  buildRequest: () => CreateSessionRequest;
}

export function useSessionDraft(): SessionDraft {
  const { data: settings } = useSettings();
  const { data: ghStatus } = useGithubStatus();
  const { data: bundles } = usePlugins();
  const models = settings?.models ?? [];
  const activeVendors = useMemo(
    () => (settings?.vendors ?? []).filter((v) => v.active),
    [settings],
  );

  const [vendor, setVendor] = useState("");
  const [model, setModel] = useState("");
  const [repos, setRepos] = useState<Map<string, string>>(new Map());
  const [skills, setSkills] = useState<Set<string>>(new Set());
  const [mcp, setMcp] = useState<Set<string>>(new Set());
  const [memorySpaces, setMemorySpaces] = useState<Set<string>>(new Set());
  const [skillsSeeded, setSkillsSeeded] = useState(false);

  // Seed model/vendor from server config, and keep them on a still-existing
  // choice if config changes.
  useEffect(() => {
    if (!settings) return;
    if (!models.some((m) => m.alias === model)) setModel(models[0]?.alias ?? "");
    if (!activeVendors.some((v) => v.name === vendor))
      setVendor(settings.defaultVendor);
  }, [settings, models, activeVendors, model, vendor]);

  // Pre-select the server's default-enabled bundles once.
  useEffect(() => {
    if (skillsSeeded || !bundles) return;
    setSkills(new Set(bundles.filter((b) => b.enabledDefault).map((b) => b.name)));
    setSkillsSeeded(true);
  }, [bundles, skillsSeeded]);

  const selectedVendor = activeVendors.find(
    (v) => v.name === (vendor || settings?.defaultVendor),
  );
  const provisions = !!selectedVendor?.capabilities?.supportsProvisioning;
  const githubConnected = !!ghStatus?.connected;

  const blockedReason = useMemo(() => {
    if (!model.trim()) return "Select a model to start.";
    if (!vendor.trim()) return "Select a runtime to start.";
    if (provisions && !githubConnected)
      return "Connect GitHub to use this runtime.";
    return null;
  }, [model, vendor, provisions, githubConnected]);

  const buildRequest = (): CreateSessionRequest => {
    const repoList: RepoConfig[] = provisions
      ? Array.from(repos.entries()).map(([fullName, ref]) => ({
          url: `https://github.com/${fullName}`,
          gitRef: ref.trim() || undefined,
        }))
      : [];
    return {
      agent: {
        model: model.trim(),
        usePlugins: provisions ? true : undefined,
        mcpServers: provisions && mcp.size ? Array.from(mcp) : undefined,
        // Not gated on `provisions`: memories are served by the server itself,
        // so they work on every vendor, including ones that can't provision.
        memorySpaces: memorySpaces.size ? Array.from(memorySpaces) : undefined,
      },
      vendor: vendor.trim() || undefined,
      repos: repoList.length ? repoList : undefined,
      plugins: provisions && skills.size ? Array.from(skills) : undefined,
    };
  };

  return {
    vendor,
    setVendor,
    model,
    setModel,
    repos,
    setRepos,
    skills,
    setSkills,
    mcp,
    setMcp,
    memorySpaces,
    setMemorySpaces,
    provisions,
    githubConnected,
    canSend: blockedReason === null,
    blockedReason,
    buildRequest,
  };
}
