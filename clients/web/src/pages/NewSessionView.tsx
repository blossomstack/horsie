import { useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { ApiRequestError } from "../api/client";
import { SessionStatusKind, type ArtifactRef } from "../api/types";
import { Composer } from "../components/Composer";
import { RailToggle } from "../components/rail";
import { SessionConfigBar } from "../components/SessionConfigBar";
import { useInvokeAgent } from "../hooks/useAgents";
import { useSessionDraft } from "../hooks/useSessionDraft";
import { useEntryCatalog } from "../hooks/useEntryCatalog";
import { useCreateSession } from "../hooks/useSessions";
import { useRunWorkflow, useWorkflows } from "../hooks/useWorkflows";
import { useTranslation } from "react-i18next";

export function NewSessionView() {
  const { t } = useTranslation();
  // The workflow page's `Run` link arrives with one preselected. A query
  // string rather than router state, so the link survives a reload.
  const [params] = useSearchParams();
  const draft = useSessionDraft(params.get("workflow") ?? "");
  // Checking and unchecking a bundle changes what `/` offers, with no session
  // and no runtime in the picture.
  const entries = useEntryCatalog(draft.skills);
  const create = useCreateSession();
  const invoke = useInvokeAgent();
  const run = useRunWorkflow();
  // Already fetched for the picker; this reads the same cache entry.
  const { data: workflows } = useWorkflows();
  const definition = workflows?.find((w) => w.name === draft.workflow);
  const navigate = useNavigate();
  const [error, setError] = useState<string | null>(null);

  // A run is created *with* its input, and so now is a session: both are one
  // call that starts the work, and neither hands a message to the next view.
  const startRun = async (text: string) => {
    const res = await run.mutateAsync({
      name: draft.workflow,
      body: draft.buildRunRequest(text),
    });
    navigate(`/sessions/${res.session.id}`);
  };

  // The message rides the create, so there is nothing left to race: the server
  // queues it into the session's inbox before this call returns, and
  // SessionView renders it from the session it fetches on mount. What used to
  // be handed over in router state — and lost on a reload — is now simply
  // already there.
  const startSession = async (text: string, artifacts: ArtifactRef[]) => {
    const res = draft.agent
      ? await invoke.mutateAsync({ name: draft.agent, body: draft.buildAgentRequest(text) })
      : await create.mutateAsync(draft.buildRequest(text, artifacts));
    // Marked as freshly started so it opens in the transcript rather than in
    // the remembered view: you just typed a message, and the answer to it is
    // the transcript. A view is remembered for the sessions you *open*, not
    // for the one you started a second ago. Router state rather than a query
    // param — the URL is a thing people send, and `?view=transcript` on every
    // new session is noise in it.
    navigate(`/sessions/${res.session.id}`, { state: { fresh: true } });
  };

  const handleSend = async (text: string, artifacts: ArtifactRef[]) => {
    setError(null);
    // A run is handed an input and a preset invocation carries only a
    // message: neither request shape has anywhere to put a file. Said out
    // loud rather than dropped, and thrown rather than returned — a rejection
    // is what puts the message *and* its attachments back in the composer.
    if (artifacts.length > 0 && (draft.workflow || draft.agent)) {
      const why = t("newSession.attachmentsUnsupported");
      setError(why);
      throw new Error(why);
    }
    try {
      if (draft.workflow) await startRun(text);
      else await startSession(text, artifacts);
    } catch (e) {
      setError(
        e instanceof ApiRequestError
          ? e.message
          : draft.workflow
            ? "Failed to start run."
            : "Failed to start session.",
      );
      // See `Composer.submit`: rethrowing is what puts the message back.
      throw e;
    }
  };

  return (
    <div className="flex h-full flex-col" data-testid="new-session-view">
      <header className="bar-scroll flex items-center gap-2 bg-panel px-4 py-3 md:hidden">
        <RailToggle />
        <span className="legend">{draft.workflow ? "New run" : "New session"}</span>
      </header>

      {/*
        The controls sit centred in the pane rather than pinned to its floor.
        This page carried a runtime roster and a paragraph explaining what a
        session is; both are gone. The roster's one useful bit — nothing is
        connected — is now a dot on the runtime key, where it sits next to the
        control that fixes it, and the paragraph was read once and skipped
        forever after. What remains is a field and the switches that configure
        it, and a lone control group at the bottom of an empty pane reads as a
        page that failed to load. Centred, it reads as a page waiting for you.
      */}
      <div
        className="flex min-h-0 flex-1 flex-col justify-center overflow-y-auto"
        data-popover-boundary
      >
        {/* The visible heading is gone by design, but the route still needs
            one — without it this page announces as untitled. */}
        <h1 className="sr-only">{draft.workflow ? "New run" : "New session"}</h1>
        {error && (
          <div className="mx-auto w-full max-w-[54rem] px-4 pb-3 sm:px-6">
            <div
              data-testid="session-error"
              className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink"
            >
              {error}
            </div>
          </div>
        )}

        {/* The config keys are icon-only, which is right for tuning a session
            and wrong for the one choice that changes what this page starts. A
            run says so in words, above the row. */}
        {draft.workflow && (
          <div
            className="mx-auto w-full max-w-[54rem] px-4 pb-2 sm:px-6"
            data-testid="workflow-run-banner"
          >
            <div className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
              <span className="legend">{t("newSession.workflowRun")}</span>
              <span className="font-mono text-[0.8125rem] text-legend">
                {draft.workflow}
              </span>
              {definition && (
                <span className="text-xs text-faint">
                  {t("newSession.stepCount", {
                    count: definition.steps.length,
                  })}{" "}
                  · {t("newSession.startsAt")}{" "}
                  <span className="text-dim">{definition.start}</span>
                </span>
              )}
            </div>
          </div>
        )}

        <SessionConfigBar mode="draft" draft={draft} />
        <Composer
          status={SessionStatusKind.Idle}
          busy={create.isPending || invoke.isPending || run.isPending}
          blockedReason={draft.blockedReason}
          entries={entries}
          idlePlaceholder={
            draft.workflow
              ? "What this run is about — the first step is handed it."
              : undefined
          }
          onSend={handleSend}
          onStop={() => {}}
          initialArtifacts={draft.artifacts}
          onArtifactsChange={draft.setArtifacts}
        />
      </div>
    </div>
  );
}
