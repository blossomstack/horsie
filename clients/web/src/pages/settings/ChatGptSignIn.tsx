import { useCallback, useEffect, useRef, useState } from "react";
import { ApiRequestError, api, type ChatGptStartedLogin } from "../../api/client";

/** Sign a `kind: "chatgpt"` provider into a ChatGPT plan.
 *
 * Device code, because the Codex OAuth client belongs to OpenAI and its
 * redirect URIs are localhost — a deployed horsie can never receive the
 * callback. Here the server talks to OpenAI outbound and the operator approves
 * on OpenAI's own site; the two halves meet at an eight-character code. Nothing
 * on this screen ever handles a ChatGPT password.
 *
 * Lives on the provider's row rather than inside its editor: connecting is not
 * an edit to the provider record, and putting it behind the pencil meant a
 * provider you had just created had to be opened a second time to be made
 * usable.
 */
export function ChatGptSignIn({
  provider,
  onChanged,
}: {
  provider: string;
  /** Signing in or out changes `hasCredential`, which the row lamp and the
   * Add-model gate both read from the settings query — without this they keep
   * showing the state from before. */
  onChanged?: () => void;
}) {
  const [accountId, setAccountId] = useState<string | null>(null);
  const [login, setLogin] = useState<ChatGptStartedLogin | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Held so the poll loop can be stopped on unmount or on success without
  // waiting out its interval.
  const timer = useRef<number | null>(null);

  const stopPolling = useCallback(() => {
    if (timer.current !== null) {
      window.clearTimeout(timer.current);
      timer.current = null;
    }
  }, []);

  useEffect(() => {
    let live = true;
    api.admin.chatgpt
      .status(provider)
      .then((s) => live && setAccountId(s.accountId ?? null))
      .catch(() => {
        /* a provider that was only just created has no status yet */
      });
    return () => {
      live = false;
      stopPolling();
    };
  }, [provider, stopPolling]);

  const poll = useCallback(
    (intervalMs: number) => {
      timer.current = window.setTimeout(() => {
        api.admin.chatgpt
          .poll(provider)
          .then((r) => {
            if (r.status === "complete") {
              setAccountId(r.accountId ?? "");
              setLogin(null);
              stopPolling();
              onChanged?.();
            } else {
              poll(intervalMs);
            }
          })
          .catch((e: unknown) => {
            setError(
              e instanceof ApiRequestError ? e.message : "The sign-in could not be checked.",
            );
            setLogin(null);
            stopPolling();
          });
      }, intervalMs);
    },
    [provider, stopPolling, onChanged],
  );

  const start = async () => {
    setBusy(true);
    setError(null);
    try {
      const started = await api.admin.chatgpt.start(provider);
      setLogin(started);
      poll(started.intervalSecs * 1000);
    } catch (e) {
      setError(
        e instanceof ApiRequestError ? e.message : "The sign-in could not be started.",
      );
    } finally {
      setBusy(false);
    }
  };

  const signOut = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.admin.chatgpt.signOut(provider);
      setAccountId(null);
      setLogin(null);
      stopPolling();
      onChanged?.();
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Could not sign out.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div data-testid="chatgpt-signin">
      {/* Recessed rather than raised: this sits inside a row that is already
          `bg-raised`, and a second raised surface on top of it reads as a
          detached card rather than as part of the row. */}
      <div className="screen p-3">
        {accountId !== null ? (
          <div
            className="flex flex-wrap items-center gap-2"
            data-testid="chatgpt-signed-in"
          >
            <span className="lamp" aria-hidden />
            <span className="legend text-current">Signed in</span>
            {accountId && <span className="chip font-mono">{accountId}</span>}
            <button
              className="key key-flat ml-auto"
              onClick={signOut}
              disabled={busy}
              data-testid="chatgpt-sign-out"
            >
              Sign out
            </button>
          </div>
        ) : login ? (
          <div className="text-sm" data-testid="chatgpt-pending">
            <p>
              Open{" "}
              <a
                className="underline"
                href={login.verificationUrl}
                target="_blank"
                rel="noreferrer"
              >
                {login.verificationUrl}
              </a>{" "}
              and enter this code:
            </p>
            <p className="my-2 font-mono text-lg tracking-widest" data-testid="chatgpt-user-code">
              {login.userCode}
            </p>
            <p className="text-xs text-dim">
              Waiting for approval… you can do this on any device. Usage draws on
              this ChatGPT plan's Codex limits.
            </p>
          </div>
        ) : (
          <div className="flex flex-wrap items-center gap-2">
            <span className="lamp lamp-off" aria-hidden />
            <span className="legend text-current">Not signed in</span>
            <button
              className="key key-go ml-auto"
              onClick={start}
              disabled={busy}
              data-testid="chatgpt-sign-in"
            >
              {busy ? "Starting…" : "Sign in with ChatGPT"}
            </button>
          </div>
        )}
        {error && (
          <p className="mt-2 text-xs text-[var(--danger)]" data-testid="chatgpt-error">
            {error}
          </p>
        )}
      </div>
    </div>
  );
}
