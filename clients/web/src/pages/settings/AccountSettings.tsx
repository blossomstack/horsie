import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { ApiRequestError, api } from "../../api/client";
import { AUTH_STATUS_KEY, useAuthStatus } from "../../hooks/useAuth";
import { ReadError } from "../../components/ReadError";
import { askConfirm } from "../../lib/confirm";
import { SettingsPane } from "./fields";
import { SettingsHeader } from "./SettingsHeader";

/** Long-lived tokens for headless vendor processes: a container, a CI runner, a
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
    // Reported under the label field.
    meta: { inlineError: true },
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

  const revoke = async (id: string, label: string) => {
    const ok = await askConfirm(
      `Revoke machine token “${label}”? Anything still using it stops connecting.`,
      "Revoke",
    );
    if (ok) remove.mutate(id);
  };

  const error =
    create.error instanceof ApiRequestError ? create.error.message : null;

  return (
    <section
      className="panel space-y-3 p-4"
      data-testid="machine-tokens"
    >
      <div>
        <h2 className="section-title">Machine tokens</h2>
        <p className="mt-0.5 text-xs text-faint">
          For runtime vendor processes that run unattended. On your own machine,
          <code className="mx-1">horsie auth login</code> is enough — use a
          token where nobody is there to approve one. A machine token connects a
          runtime and can do nothing else: it cannot read sessions, change
          settings, or create another token.
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
        {tokens.isLoading && <p className="text-xs text-faint">Loading…</p>}
        {/* A revoked-looking list is the worst possible failure mode here: the
            reflex is to mint a replacement token, which does nothing about a
            server that is not answering. */}
        {tokens.isError && (
          <ReadError
            what="machine tokens"
            error={tokens.error}
            testId="tokens-error"
          />
        )}
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
              <div className="text-[0.6875rem] text-faint">
                {t.lastUsedAt ? "in use" : "never used"}
              </div>
            </div>
            <button
              className="key shrink-0 text-xs"
              data-testid={`token-revoke-${t.label}`}
              // Nothing here says which machine holds a token, so a revoke is
              // both irreversible and hard to undo by hand: the confirm names
              // the label and what stops working.
              onClick={() => void revoke(t.id, t.label)}
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
    // Reported beside the password fields.
    meta: { inlineError: true },
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
      // Clearing this server's session is only half of it when identity lives
      // elsewhere: the provider holds one too, and leaving it alive means
      // "sign out" did not sign anyone out of a shared machine.
      if (s.external && s.logoutUrl) window.location.assign(s.logoutUrl);
    },
  });

  if (!status?.enabled) {
    return (
      <div className="flex h-full flex-col">
        <SettingsHeader title="Account" desc="Sign-in for this server." />
        <SettingsPane>
          <p
            data-testid="account-disabled"
            className="panel p-4 text-sm text-dim"
          >
            Authentication is disabled on this deployment, so there is no
            account to manage. Anyone who can reach this server has full access.
          </p>
        </SettingsPane>
      </div>
    );
  }

  const error =
    change.error instanceof ApiRequestError ? change.error.message : null;

  return (
    <div className="flex h-full flex-col">
      <SettingsHeader title="Account" desc="Sign-in for this server." />
      <SettingsPane>
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
        {status.external ? (
          <p
            data-testid="account-external"
            className="panel p-4 text-sm text-dim"
          >
            Sign-in for this server is managed elsewhere, so there is no
            password to change here.
          </p>
        ) : (
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
        )}
        <button
          type="button"
          data-testid="logout"
          className="key"
          onClick={() => logout.mutate()}
        >
          Sign out
        </button>
        <MachineTokens />
      </SettingsPane>
    </div>
  );
}
