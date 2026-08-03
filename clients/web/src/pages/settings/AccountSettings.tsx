import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { ApiRequestError, api } from "../../api/client";
import { AUTH_STATUS_KEY, useAuthStatus } from "../../hooks/useAuth";
import { SettingsHeader } from "./SettingsHeader";

/** Long-lived tokens for headless vendor agents: a container, a CI runner, a
 *  machine with nobody to approve a device code. */
function MachineTokens() {
  const qc = useQueryClient();
  const [label, setLabel] = useState("");
  const [fresh, setFresh] = useState<string | null>(null);

  const tokens = useQuery({
    queryKey: ["auth", "tokens"],
    queryFn: () => api.auth.listTokens(),
  });
  const create = useMutation({
    mutationFn: () => api.auth.createToken(label),
    onSuccess: (created) => {
      setLabel("");
      // Shown once: only the hash is stored, so there is nothing to show later.
      setFresh(created.token);
      void qc.invalidateQueries({ queryKey: ["auth", "tokens"] });
    },
  });
  const remove = useMutation({
    mutationFn: (id: string) => api.auth.deleteToken(id),
    onSuccess: () =>
      void qc.invalidateQueries({ queryKey: ["auth", "tokens"] }),
  });

  const error =
    create.error instanceof ApiRequestError ? create.error.message : null;

  return (
    <section
      className="panel max-w-2xl space-y-3 p-4"
      data-testid="machine-tokens"
    >
      <div>
        <h2 className="text-sm font-semibold text-legend">Machine tokens</h2>
        <p className="mt-0.5 text-xs text-faint">
          For runtime vendor agents that run unattended. On your own machine,
          <code className="mx-1">horsie auth login</code> is enough — use a
          token where nobody is there to approve one.
        </p>
      </div>

      <form
        className="flex gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          create.mutate();
        }}
      >
        <input
          className="field flex-1"
          data-testid="token-label"
          placeholder="What machine is this for?"
          value={label}
          onChange={(e) => setLabel(e.target.value)}
        />
        <button
          type="submit"
          className="key key-go shrink-0"
          data-testid="token-create"
          disabled={!label.trim() || create.isPending}
        >
          Create
        </button>
      </form>
      {error && (
        <p data-testid="token-error" className="text-xs text-red-ink">
          {error}
        </p>
      )}

      {fresh && (
        <div className="space-y-1" data-testid="token-secret">
          <p className="text-xs text-lamp-ok">
            Copy this now — it will not be shown again.
          </p>
          <code className="block break-all rounded-[var(--radius-control)] border p-2 font-mono text-xs">
            {fresh}
          </code>
        </div>
      )}

      <div className="space-y-1.5">
        {tokens.data?.length === 0 && (
          <p className="text-xs text-faint">No machine tokens yet.</p>
        )}
        {tokens.data?.map((t) => (
          <div
            key={t.id}
            className="flex items-center justify-between gap-3 rounded-[var(--radius-control)] border px-3 py-2"
            data-testid={`token-row-${t.label}`}
          >
            <div className="min-w-0">
              <div className="truncate text-sm text-legend">{t.label}</div>
              <div className="text-[11px] text-faint">
                {t.lastUsedAt ? "in use" : "never used"}
              </div>
            </div>
            <button
              className="key shrink-0 text-xs"
              data-testid={`token-revoke-${t.label}`}
              onClick={() => remove.mutate(t.id)}
              disabled={remove.isPending}
            >
              Revoke
            </button>
          </div>
        ))}
      </div>
    </section>
  );
}

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
            className="panel p-4 text-sm text-dim"
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
            className="panel p-4 text-sm text-legend"
          >
            This server is still using the password it generated on first boot.
            Change it below — that also deletes the{" "}
            <code>initial-admin-password</code> file from the state directory.
          </p>
        )}
        <form
          data-testid="password-form"
          className="panel max-w-sm space-y-3 p-4"
          onSubmit={(e) => {
            e.preventDefault();
            change.mutate();
          }}
        >
          <input
            className="field"
            type="password"
            autoComplete="current-password"
            data-testid="current-password"
            placeholder="Current password"
            value={current}
            onChange={(e) => setCurrent(e.target.value)}
          />
          <input
            className="field"
            type="password"
            autoComplete="new-password"
            data-testid="new-password"
            placeholder="New password (8 characters or more)"
            value={next}
            onChange={(e) => setNext(e.target.value)}
          />
          {error && (
            <p data-testid="password-error" className="text-xs text-red-ink">
              {error}
            </p>
          )}
          {change.isSuccess && (
            <p data-testid="password-saved" className="text-xs text-lamp-ok">
              Password changed. Other browsers have been signed out.
            </p>
          )}
          <button
            type="submit"
            data-testid="password-submit"
            className="key key-go"
            disabled={change.isPending || !current || !next}
          >
            Change password
          </button>
        </form>
        <button
          type="button"
          data-testid="logout"
          className="key"
          onClick={() => logout.mutate()}
        >
          Sign out
        </button>
        <MachineTokens />
      </div>
    </div>
  );
}
