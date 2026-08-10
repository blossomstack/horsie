import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { WorkflowInput, WorkflowRunRequest } from "../api/types";
import { qk } from "./useSessions";

export const workflowKeys = {
  all: ["workflows"] as const,
  one: (name: string) => ["workflows", name] as const,
  runs: (name: string) => ["workflows", name, "runs"] as const,
  graph: (sessionId: string) => ["workflows", "graph", sessionId] as const,
};

/** All workflow definitions. */
export function useWorkflows() {
  return useQuery({
    queryKey: workflowKeys.all,
    queryFn: () => api.workflows.list(),
  });
}

export function useWorkflow(name: string | undefined) {
  return useQuery({
    queryKey: name ? workflowKeys.one(name) : ["workflows", "none"],
    queryFn: () => api.workflows.get(name as string),
    enabled: !!name,
  });
}

/** A workflow's runs, newest first. Polled while the page is open: a run's
 * status lives in its session, not in the definition. */
export function useWorkflowRuns(name: string | undefined) {
  return useQuery({
    queryKey: name ? workflowKeys.runs(name) : ["workflows", "none", "runs"],
    queryFn: () => api.workflows.runs(name as string),
    enabled: !!name,
    refetchInterval: 5_000,
    select: (r) => r.sessions,
  });
}

/** One run, projected onto its graph.
 *
 * Polled rather than streamed. The session's SSE stream says *that* something
 * changed but not which step, and the projection is one small request — so a
 * poll is both simpler and honest about being a snapshot. It stops once the
 * run reaches a terminal state, because nothing can change after that without
 * a retry, which invalidates this query itself. */
export function useWorkflowRun(sessionId: string | undefined) {
  return useQuery({
    queryKey: sessionId ? workflowKeys.graph(sessionId) : ["workflows", "graph", "none"],
    queryFn: () => api.workflows.graph(sessionId as string),
    enabled: !!sessionId,
    refetchInterval: (query) => {
      const status = query.state.data?.status.type;
      return status === "Finished" || status === "Failed" ? false : 2_000;
    },
  });
}

export function useCreateWorkflow() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: WorkflowInput) => api.workflows.create(body),
    // The editor renders the failure beside its Save button.
    meta: { inlineError: true },
    onSuccess: () => qc.invalidateQueries({ queryKey: workflowKeys.all }),
  });
}

export function useUpdateWorkflow() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, body }: { name: string; body: WorkflowInput }) =>
      api.workflows.update(name, body),
    // As above: reported inline by the editor.
    meta: { inlineError: true },
    onSuccess: (_r, { name }) => {
      qc.invalidateQueries({ queryKey: workflowKeys.all });
      qc.invalidateQueries({ queryKey: workflowKeys.one(name) });
    },
  });
}

export function useDeleteWorkflow() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.workflows.remove(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: workflowKeys.all }),
  });
}

/** Start a run. The session list gains a row, so it is invalidated too. */
export function useRunWorkflow() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, body }: { name: string; body: WorkflowRunRequest }) =>
      api.workflows.run(name, body),
    onSuccess: (_r, { name }) => {
      qc.invalidateQueries({ queryKey: workflowKeys.runs(name) });
      qc.invalidateQueries({ queryKey: qk.sessions });
    },
  });
}

/** Re-run one execution of a step. */
export function useRetryStep(sessionId: string | undefined) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (stepIndex: number) => api.workflows.retry(sessionId as string, stepIndex),
    onSuccess: () => {
      if (sessionId) qc.invalidateQueries({ queryKey: workflowKeys.graph(sessionId) });
    },
  });
}
