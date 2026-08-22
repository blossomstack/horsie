import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { ApiRequestError, api } from "../api/client";
import { AUTH_STATUS_KEY } from "../hooks/useAuth";

/**
 * Shown instead of the app whenever the server says authentication is on and
 * this browser is not authenticated. For most people this is the first horsie
 * screen they ever see, so it says where the generated password lives rather
 * than leaving them to search the docs for it.
 */
export function LoginPage() {
  const qc = useQueryClient();
  const [password, setPassword] = useState("");
  const login = useMutation({
    mutationFn: () => api.auth.login(password),
    // A wrong password belongs under the password field, not in a corner.
    meta: { inlineError: true },
    onSuccess: (status) => {
      qc.setQueryData(AUTH_STATUS_KEY, status);
      void qc.invalidateQueries();
    },
  });

  const message =
    login.error instanceof ApiRequestError
      ? login.error.message
      : login.error
        ? "Could not sign in. Check the server is still running, then try again."
        : null;

  return (
    <div className="flex h-full items-center justify-center p-6">
      <form
        data-testid="login-form"
        className="panel w-full max-w-[22rem] p-6 shadow-[var(--panel-lift)]"
        onSubmit={(e) => {
          e.preventDefault();
          login.mutate();
        }}
      >
        <div className="flex items-center gap-2.5">
          <span
            aria-hidden
            className="flex h-7 w-7 items-center justify-center rounded-[4px] bg-accent font-mono text-sm font-bold text-accent-ink shadow-[var(--cap-lift)]"
          >
            h
          </span>
          <span className="font-mono text-sm font-semibold tracking-[0.16em] text-legend">
            HORSIE
          </span>
        </div>

        <h1 className="mt-5 text-[0.9375rem] font-semibold text-legend">Sign in</h1>
        <p className="mt-1 text-xs leading-relaxed text-faint">
          This server requires a password. On first boot horsie writes a
          generated one to{" "}
          <code className="font-mono text-dim">
            initial-admin-password
          </code>{" "}
          in its state directory.
        </p>

        <label className="mt-4 block">
          <span className="legend mb-1 block">Password</span>
          <input
            className="field"
            type="password"
            autoFocus
            autoComplete="current-password"
            data-testid="login-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </label>

        {message && (
          <p
            data-testid="login-error"
            className="mt-3 rounded-[var(--radius-control)] border border-red bg-red-quiet px-2.5 py-2 text-xs leading-relaxed text-red-ink"
          >
            {message}
          </p>
        )}

        <button
          type="submit"
          data-testid="login-submit"
          className="key key-go mt-4 w-full justify-center"
          disabled={login.isPending || password.length === 0}
        >
          {login.isPending ? "Signing in…" : "Sign in"}
        </button>
      </form>
    </div>
  );
}
