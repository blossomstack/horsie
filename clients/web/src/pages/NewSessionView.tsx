import { Sparkles } from "lucide-react";
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { ApiRequestError } from "../api/client";
import { SessionStatusKind } from "../api/types";
import { Composer } from "../components/Composer";
import { EmptyState } from "../components/EmptyState";
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
      <div className="flex-1 overflow-y-auto">
        <EmptyState icon={<Sparkles size={24} />} title="New chat">
          Pick a runtime below, then send a message to start. For a remote
          runtime you can also select repositories, skills, and MCP servers.
        </EmptyState>
      </div>

      {error && (
        <div className="mx-auto w-full max-w-3xl px-4">
          <div
            data-testid="session-error"
            className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error"
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
  );
}
