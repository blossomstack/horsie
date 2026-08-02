import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { ApiRequestError, api } from "../../api/client";
import { AUTH_STATUS_KEY, useAuthStatus } from "../../hooks/useAuth";
import { SettingsHeader } from "./SettingsHeader";

/** Password change and sign-out for the single admin account. */
export function AccountSettings() {
  const qc = useQueryClient();
  const { data: status } = useAuthStatus();
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");

  const change = useMutation({
    mutationFn: () =>
      api.auth.changePassword({ currentPassword: current, newPassword: next }),
    onSuccess: (s) => {
      setCurrent("");
      setNext("");
      qc.setQueryData(AUTH_STATUS_KEY, s);
    },
  });

  const logout = useMutation({
    mutationFn: () => api.auth.logout(),
    onSuccess: (s) => {
      qc.setQueryData(AUTH_STATUS_KEY, s);
      void qc.invalidateQueries();
    },
  });

  if (!status?.enabled) {
    return (
      <div className="flex h-full flex-col">
        <SettingsHeader title="Account" desc="Sign-in for this server." />
        <div className="p-6">
          <p
            data-testid="account-disabled"
            className="card p-4 text-sm text-muted"
          >
            Authentication is disabled on this deployment, so there is no
            account to manage. Anyone who can reach this server has full access.
          </p>
        </div>
      </div>
    );
  }

  const error =
    change.error instanceof ApiRequestError ? change.error.message : null;

  return (
    <div className="flex h-full flex-col">
      <SettingsHeader title="Account" desc="Sign-in for this server." />
      <div className="space-y-6 p-6">
        {status.mustChangePassword && (
          <p
            data-testid="account-must-change"
            className="card p-4 text-sm text-text"
          >
            This server is still using the password it generated on first boot.
            Change it below — that also deletes the{" "}
            <code>initial-admin-password</code> file from the state directory.
          </p>
        )}
        <form
          data-testid="password-form"
          className="card max-w-sm space-y-3 p-4"
          onSubmit={(e) => {
            e.preventDefault();
            change.mutate();
          }}
        >
          <input
            className="input"
            type="password"
            autoComplete="current-password"
            data-testid="current-password"
            placeholder="Current password"
            value={current}
            onChange={(e) => setCurrent(e.target.value)}
          />
          <input
            className="input"
            type="password"
            autoComplete="new-password"
            data-testid="new-password"
            placeholder="New password (8 characters or more)"
            value={next}
            onChange={(e) => setNext(e.target.value)}
          />
          {error && (
            <p data-testid="password-error" className="text-xs text-error">
              {error}
            </p>
          )}
          {change.isSuccess && (
            <p data-testid="password-saved" className="text-xs text-success">
              Password changed. Other browsers have been signed out.
            </p>
          )}
          <button
            type="submit"
            data-testid="password-submit"
            className="btn-primary"
            disabled={change.isPending || !current || !next}
          >
            Change password
          </button>
        </form>
        <button
          type="button"
          data-testid="logout"
          className="btn-outline"
          onClick={() => logout.mutate()}
        >
          Sign out
        </button>
      </div>
    </div>
  );
}
