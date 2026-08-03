import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { ApiRequestError } from "../../api/client";
import {
  useGithubAppConfig,
  useGithubStatus,
  useSaveGithubAppConfig,
} from "../../hooks/useGithub";
import { usePublishDirty } from "../settings/dirty";
import { Section, SettingsPane, TextField } from "../settings/fields";
import { SettingsHeader } from "../settings/SettingsHeader";

/**
 * The GitHub App's credentials.
 *
 * Separated from Settings → Integrations, which now holds only the act of
 * connecting an account. These four values are server-wide registration
 * details entered once by whoever runs the server; the connect button is used
 * by whoever uses it. Mixing them put a client secret and a private key on the
 * same panel as a button most visits are there to press.
 *
 * This is a presentation split. `/api/github/app-config` is unchanged, and
 * whether it is admin-gated is a property of the server, not of this page
 * living under /admin.
 */
export function GithubAppPage() {
  const { data: status } = useGithubStatus();
  const { data: cfg } = useGithubAppConfig();
  const save = useSaveGithubAppConfig();

  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [appId, setAppId] = useState("");
  const [privateKey, setPrivateKey] = useState("");
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  // The only batched form left in the product, so the nav guard exists for it.
  usePublishDirty(dirty);

  // Seed from the stored config once, until the user edits it. Secrets are
  // write-only: the server returns only whether one is set.
  useEffect(() => {
    if (!cfg || dirty) return;
    setClientId(cfg.clientId ?? "");
    setAppId(cfg.appId != null ? String(cfg.appId) : "");
  }, [cfg, dirty]);

  const submit = async () => {
    setError(null);
    try {
      await save.mutateAsync({
        clientId: clientId.trim(),
        clientSecret: clientSecret === "" ? undefined : clientSecret,
        appId: appId.trim() === "" ? undefined : Number(appId),
        privateKey: privateKey === "" ? undefined : privateKey,
      });
      setClientSecret("");
      setPrivateKey("");
      setDirty(false);
      setSaved(true);
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Failed to save.");
    }
  };

  const touch = () => {
    setDirty(true);
    setSaved(false);
  };

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <SettingsHeader
        title="GitHub App"
        desc="Registration details for the GitHub App this server acts as. Set once; users then connect their own accounts from Settings → Integrations."
        dirty={dirty}
        saved={saved}
        saving={save.isPending}
        onSave={submit}
        onDiscard={() => {
          setClientSecret("");
          setPrivateKey("");
          setClientId(cfg?.clientId ?? "");
          setAppId(cfg?.appId != null ? String(cfg.appId) : "");
          setDirty(false);
        }}
      />

      <SettingsPane>
        <Section
          title="Credentials"
          desc="From the app's page on GitHub. The secret and private key are write-only — the server reports only whether each one is set."
        >
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <TextField
              label="Client ID"
              value={clientId}
              onChange={(v) => {
                setClientId(v);
                touch();
              }}
            />
            <TextField
              label="Client secret"
              type="password"
              value={clientSecret}
              onChange={(v) => {
                setClientSecret(v);
                touch();
              }}
              placeholder={
                cfg?.hasClientSecret ? "•••• stored — blank keeps it" : "Not set"
              }
            />
            <TextField
              label="App ID"
              value={appId}
              onChange={(v) => {
                setAppId(v);
                touch();
              }}
            />
            <TextField
              label="Private key (PEM or base64)"
              type="password"
              value={privateKey}
              onChange={(v) => {
                setPrivateKey(v);
                touch();
              }}
              placeholder={
                cfg?.hasPrivateKey ? "•••• stored — blank keeps it" : "Not set"
              }
            />
          </div>

          {error && (
            <div className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
              {error}
            </div>
          )}

          <p className="flex items-center gap-2 text-xs leading-relaxed text-faint">
            <span
              className={status?.appConfigured ? "lamp text-lamp-ok" : "lamp lamp-off"}
              aria-hidden
            />
            {status?.appConfigured ? (
              <>
                App configured.{" "}
                <Link
                  to="/settings/integrations"
                  className="text-amber-ink underline underline-offset-2"
                >
                  Connect an account
                </Link>
              </>
            ) : (
              "Not configured yet — sessions cannot clone repositories until it is."
            )}
          </p>
        </Section>
      </SettingsPane>
    </div>
  );
}
