import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { ApiRequestError, api } from "../../api/client";
import { AUTH_STATUS_KEY, useAuthStatus } from "../../hooks/useAuth";
import { ReadError } from "../../components/ReadError";
import { askConfirm } from "../../lib/confirm";
import { SettingsPage } from "./fields";
import { Trans, useTranslation } from "react-i18next";

/** Long-lived tokens for headless vendor processes: a container, a CI runner, a
 *  machine with nobody to approve a device code. */
function MachineTokens() {
  const { t } = useTranslation();
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
      t("account.confirmRevoke", { label }),
      t("account.revoke"),
    );
    if (ok) remove.mutate(id);
  };

  const error =
    create.error instanceof ApiRequestError ? create.error.message : null;

  return (
    <section
      className="section space-y-3"
      data-testid="machine-tokens"
    >
      <div>
        <h2 className="section-title">{t("account.tokens")}</h2>
        <p className="mt-0.5 text-xs text-faint">
          <Trans
            i18nKey="account.tokensDesc"
            components={{ cmd: <code className="mx-1" /> }}
          />
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
          placeholder={t("account.tokenLabelPlaceholder")}
          value={label}
          onChange={(e) => setLabel(e.target.value)}
        />
        <button
          type="submit"
          className="key key-go shrink-0"
          data-testid="token-create"
          disabled={!label.trim() || create.isPending}
        >
          {t("common.create")}
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
{t("account.copyNow")}
          </p>
          <code className="block break-all rounded-[var(--radius-control)] p-2 font-mono text-xs">
            {fresh}
          </code>
        </div>
      )}

      <div className="space-y-1.5">
        {tokens.isLoading && (
          <p className="text-xs text-faint">{t("common.loading")}</p>
        )}
        {/* A revoked-looking list is the worst possible failure mode here: the
            reflex is to mint a replacement token, which does nothing about a
            server that is not answering. */}
        {tokens.isError && (
          <ReadError
            what={t("account.tokensWhat")}
            error={tokens.error}
            testId="tokens-error"
          />
        )}
        {tokens.data?.length === 0 && (
          <p className="text-xs text-faint">{t("account.noTokens")}</p>
        )}
        {tokens.data?.map((token) => (
          <div
            key={token.id}
            className="flex items-center justify-between gap-3 rounded-[var(--radius-control)] px-3 py-2"
            data-testid={`token-row-${token.label}`}
          >
            <div className="min-w-0">
              <div className="truncate text-sm text-legend">{token.label}</div>
              <div className="text-[0.6875rem] text-faint">
                {token.lastUsedAt ? t("account.inUse") : t("account.neverUsed")}
              </div>
            </div>
            <button
              className="key shrink-0 text-xs"
              data-testid={`token-revoke-${token.label}`}
              // Nothing here says which machine holds a token, so a revoke is
              // both irreversible and hard to undo by hand: the confirm names
              // the label and what stops working.
              onClick={() => void revoke(token.id, token.label)}
              disabled={remove.isPending}
            >
              {t("account.revoke")}
            </button>
          </div>
        ))}
      </div>
    </section>
  );
}

/** Password change and sign-out for the single admin account. */
export function AccountSettings() {
  const { t } = useTranslation();
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
      <SettingsPage title={t("settingsNav.account")}>
          <p
            data-testid="account-disabled"
            className="section  text-sm text-dim"
          >
{t("account.disabled")}
          </p>
      </SettingsPage>
    );
  }

  const error =
    change.error instanceof ApiRequestError ? change.error.message : null;

  return (
    <SettingsPage title={t("settingsNav.account")}>
        {status.mustChangePassword && (
          <p
            data-testid="account-must-change"
            className="section  text-sm text-legend"
          >
            <Trans i18nKey="account.mustChange" components={{ file: <code /> }} />
          </p>
        )}
        {status.external ? (
          <p
            data-testid="account-external"
            className="section  text-sm text-dim"
          >
{t("account.external")}
          </p>
        ) : (
          <form
            data-testid="password-form"
            className="section max-w-sm space-y-3"
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
              placeholder={t("account.currentPassword")}
              value={current}
              onChange={(e) => setCurrent(e.target.value)}
            />
            <input
              className="field"
              type="password"
              autoComplete="new-password"
              data-testid="new-password"
              placeholder={t("account.newPassword")}
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
{t("account.passwordChanged")}
              </p>
            )}
            <button
              type="submit"
              data-testid="password-submit"
              className="key key-go"
              disabled={change.isPending || !current || !next}
            >
              {t("account.changePassword")}
            </button>
          </form>
        )}
        <button
          type="button"
          data-testid="logout"
          className="key"
          onClick={() => logout.mutate()}
        >
          {t("chatgpt.signOut")}
        </button>
        <MachineTokens />
      </SettingsPage>
  );
}
