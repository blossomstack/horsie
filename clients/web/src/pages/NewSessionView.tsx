import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { ApiRequestError } from "../api/client";
import { SessionStatusKind } from "../api/types";
import { Composer } from "../components/Composer";
import { RailToggle } from "../components/rail";
import { SessionConfigBar } from "../components/SessionConfigBar";
import { useSessionDraft } from "../hooks/useSessionDraft";
import { useCreateSession } from "../hooks/useSessions";
import type { PendingFirstMessageState } from "./SessionView";

export function NewSessionView() {
  const draft = useSessionDraft();
  const create = useCreateSession();
  const navigate = useNavigate();
  const [error, setError] = useState<string | null>(null);

  const handleSend = async (text: string) => {
    setError(null);
    try {
      const res = await create.mutateAsync(draft.buildRequest());
      // Navigate first so SessionView fully mounts (its own session fetch +
      // live SSE connect) before the first message is sent — sending it here
      // instead raced the server's async provisioning under CI's slower
      // scheduling, sometimes leaving the turn stuck. SessionView picks this
      // up on mount via router state and sends it through the normal
      // useSendMessage path, same as every later message.
      const state: PendingFirstMessageState = { pendingFirstMessage: text };
      navigate(`/sessions/${res.session.id}`, { state });
    } catch (e) {
      setError(
        e instanceof ApiRequestError ? e.message : "Failed to start session.",
      );
    }
  };

  return (
    <div className="flex h-full flex-col" data-testid="new-session-view">
      <header className="flex items-center gap-2 border-b bg-panel px-4 py-3 md:hidden">
        <RailToggle />
        <span className="legend">New session</span>
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
      <div className="flex min-h-0 flex-1 flex-col justify-center overflow-y-auto">
        {/* The visible heading is gone by design, but the route still needs
            one — without it this page announces as untitled. */}
        <h1 className="sr-only">New session</h1>
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

        <SessionConfigBar mode="draft" draft={draft} />
        <Composer
          status={SessionStatusKind.Idle}
          busy={create.isPending}
          blockedReason={draft.blockedReason}
          onSend={handleSend}
          onStop={() => {}}
        />
      </div>
    </div>
  );
}
