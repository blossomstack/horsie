import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { ApiRequestError, api } from "../api/client";
import { AUTH_STATUS_KEY } from "../hooks/useAuth";

/**
 * Shown instead of the app whenever the server says authentication is on and
 * this browser is not authenticated.
 */
export function LoginPage() {
  const qc = useQueryClient();
  const [password, setPassword] = useState("");
  const login = useMutation({
    mutationFn: () => api.auth.login(password),
    onSuccess: (status) => {
      qc.setQueryData(AUTH_STATUS_KEY, status);
      void qc.invalidateQueries();
    },
  });

  const message =
    login.error instanceof ApiRequestError
      ? login.error.message
      : login.error
        ? "Could not sign in."
        : null;

  return (
    <div className="flex h-full items-center justify-center p-6">
      <form
        data-testid="login-form"
        className="card w-full max-w-sm space-y-4 p-6"
        onSubmit={(e) => {
          e.preventDefault();
          login.mutate();
        }}
      >
        <div>
          <h1 className="text-[15px] font-semibold text-text">Sign in</h1>
          <p className="mt-0.5 text-xs text-faint">
            This horsie server requires a password.
          </p>
        </div>
        <input
          className="input"
          type="password"
          autoFocus
          autoComplete="current-password"
          data-testid="login-password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          placeholder="Password"
        />
        {message && (
          <p data-testid="login-error" className="text-xs text-error">
            {message}
          </p>
        )}
        <button
          type="submit"
          data-testid="login-submit"
          className="btn-primary w-full justify-center"
          disabled={login.isPending || password.length === 0}
        >
          {login.isPending ? "Signing in…" : "Sign in"}
        </button>
      </form>
    </div>
  );
}
