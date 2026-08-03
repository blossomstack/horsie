import { useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { ApiRequestError } from "../api/client";
import { SessionStatusKind } from "../api/types";
import { Composer } from "../components/Composer";
import { RailToggle } from "../components/rail";
import { SessionConfigBar } from "../components/SessionConfigBar";
import { useSettings } from "../hooks/useSettings";
import { useSessionDraft } from "../hooks/useSessionDraft";
import { useCreateSession } from "../hooks/useSessions";
import type { PendingFirstMessageState } from "./SessionView";

/** Which machines can actually run this session, read off the live vendor
 * roster. A self-hoster's first question on this screen is "is my laptop
 * agent connected?", and the answer used to require a trip to Settings. */
function RuntimeRoster() {
  const { data: settings } = useSettings();
  const vendors = settings?.vendors ?? [];
  return (
    <section className="panel p-4" data-testid="runtime-roster">
      <div className="flex items-baseline justify-between gap-3">
        <h2 className="legend">Runtimes connected</h2>
        <span className="readout text-[13px]">{vendors.length}</span>
      </div>
      {vendors.length === 0 ? (
        <p className="mt-3 max-w-prose text-sm leading-relaxed text-dim">
          No runtime agent is connected, so a session can’t run a turn yet. Run{" "}
          <code className="font-mono text-legend">
            horsie connect --workspace .
          </code>{" "}
          on the machine holding your code, or{" "}
          <Link to="/settings/runtimes" className="text-amber-ink underline underline-offset-2">
            review runtimes in Settings
          </Link>
          .
        </p>
      ) : (
        <ul className="mt-3 flex flex-wrap gap-x-5 gap-y-2">
          {vendors.map((v) => (
            <li key={v.name} className="flex items-center gap-2">
              <span className="lamp text-lamp-ok" aria-hidden />
              <span className="font-mono text-[12px] text-legend">{v.name}</span>
              <span className="legend">
                {v.capabilities.supportsProvisioning ? "Provisions" : "Own dirs"}
              </span>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

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
        <span className="font-mono text-[11px] font-semibold uppercase tracking-[0.12em] text-legend">
          New session
        </span>
      </header>

      {/* Two panels with the gap between them, rather than one bottom-anchored
          stack under an unpainted field: the console's own state sits at the
          top where a status band belongs, and the launch block stays next to
          the controls it describes. */}
      <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
        <div className="mx-auto w-full max-w-[54rem] px-4 pt-5 sm:px-6">
          <RuntimeRoster />
        </div>
        <div className="mx-auto w-full max-w-[54rem] px-4 pb-8 pt-10 sm:px-6 mt-auto">
          <h1 className="font-mono text-[13px] font-semibold uppercase tracking-[0.12em] text-legend">
            New session
          </h1>
          <p className="mt-2 max-w-prose text-sm leading-relaxed text-dim">
            Set the model and runtime below, then send your first message — the
            session is created when you do, and everything after that is
            journaled on the server. Runtimes that provision a workspace can
            also check out repositories and load skills and MCP servers.
          </p>
        </div>
      </div>

      {error && (
        <div className="mx-auto w-full max-w-[54rem] px-4 sm:px-6">
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
  );
}
