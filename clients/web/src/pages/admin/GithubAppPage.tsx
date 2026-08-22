import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { ApiRequestError } from "../../api/client";
import {
  useGithubAppConfig,
  useGithubStatus,
  useSaveGithubAppConfig,
} from "../../hooks/useGithub";
import { usePublishDirty } from "../settings/dirty";
import {
  Section,
  SettingsPage,
  TextAreaField,
  TextField,
} from "../settings/fields";

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
  const [callbackBase, setCallbackBase] = useState("");
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
    setCallbackBase(cfg.callbackBase ?? "");
  }, [cfg, dirty]);

  // What this form will not send. It used to send anything: `abc` in App ID
  // became `Number("abc")` → NaN → JSON `null`, stored as NULL, and came back
  // as a blank field with a green "SAVED" and nothing saying the input had
  // been discarded.
  const appIdError =
    appId.trim() !== "" && !/^\d+$/.test(appId.trim())
      ? "The App ID is the number on the app's page on GitHub."
      : null;
  const callbackBaseError =
    callbackBase.trim() !== "" && !/^https?:\/\/[^\s]+$/.test(callbackBase.trim())
      ? "An absolute URL, e.g. https://horsie.example.com."
      : null;
  const clientIdError =
    clientId.trim() === "" ? "The client id is what identifies the app." : null;
  const invalid = appIdError ?? callbackBaseError ?? clientIdError;

  const submit = async () => {
    setError(null);
    if (invalid) return setError(invalid);
    try {
      await save.mutateAsync({
        clientId: clientId.trim(),
        clientSecret: clientSecret === "" ? undefined : clientSecret,
        appId: appId.trim() === "" ? undefined : Number(appId),
        privateKey: privateKey === "" ? undefined : privateKey,
        // Submitted even when blank. The save is a full replacement, so
        // omitting this field is what silently wiped it on every save — and
        // it is the only override for a wrong `redirect_uri`, so one
        // unrelated edit here used to re-break the whole OAuth flow.
        callbackBase: callbackBase.trim() === "" ? undefined : callbackBase.trim(),
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
    <SettingsPage
        title="GitHub App"
        desc="Registration details for the GitHub App this server acts as. Set once; users then connect their own accounts from Settings → Integrations."
        dirty={dirty}
        saved={saved}
        saving={save.isPending}
        saveBlocked={invalid !== null}
        onSave={submit}
        onDiscard={() => {
          setClientSecret("");
          setPrivateKey("");
          setClientId(cfg?.clientId ?? "");
          setAppId(cfg?.appId != null ? String(cfg.appId) : "");
          setCallbackBase(cfg?.callbackBase ?? "");
          setDirty(false);
        }}
    >
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
              invalid={dirty ? clientIdError : null}
              testId="github-client-id"
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
              invalid={appIdError}
              hint="The number on the app's page on GitHub."
              testId="github-app-id"
            />
            {/* A textarea, not an <input type="password">. Chrome collapses
                newlines to spaces when pasting into a single-line input, and
                the documented primary format — a PEM — is newline-delimited by
                definition, so what was stored was never what was copied. (The
                key horsie parses survives that mangling; `openssl rsa -check`
                does not, so the value on screen and the value in the field
                disagreed about being valid.) A key is not a password anyway:
                it is pasted once, and seeing that it pasted whole is worth
                more than the dots — and the save now parses it either way. */}
            <TextAreaField
              label="Private key (PEM or base64)"
              value={privateKey}
              rows={4}
              onChange={(v) => {
                setPrivateKey(v);
                touch();
              }}
              placeholder={
                cfg?.hasPrivateKey ? "•••• stored — blank keeps it" : "Not set"
              }
              hint="Paste the whole PEM, BEGIN and END lines included."
              testId="github-private-key"
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
                  className="text-live-ink underline underline-offset-2"
                >
                  Connect an account
                </Link>
              </>
            ) : (
              "Not configured yet — sessions cannot clone repositories until it is."
            )}
          </p>
        </Section>

        <Section
          title="Callback"
          desc="Where GitHub sends users back after they authorize. horsie derives this from the request, honouring X-Forwarded-Proto, so a correctly configured reverse proxy needs nothing here. Set it when horsie cannot see its own public address — a proxy that does not forward the scheme, or a path prefix."
        >
          <TextField
            label="Callback base URL"
            value={callbackBase}
            onChange={(v) => {
              setCallbackBase(v);
              touch();
            }}
            placeholder="https://horsie.example.com"
            invalid={callbackBaseError}
            testId="github-callback-base"
          />
        </Section>
      </SettingsPage>
  );
}
