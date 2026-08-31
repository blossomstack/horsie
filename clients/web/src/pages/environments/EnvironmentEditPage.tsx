import { useMemo, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { Plus, Trash2 } from "lucide-react";
import { ApiRequestError } from "../../api/client";
import { RailToggle } from "../../components/rail";
import { ReadError } from "../../components/ReadError";
import type {
  EnvironmentView,
  EnvVar,
  ProvisionStep,
  RepoConfig,
} from "../../api/types";
import {
  useCreateEnvironment,
  useEnvironment,
  useUpdateEnvironment,
} from "../../hooks/useEnvironments";
import { useGithubRepos, useGithubStatus } from "../../hooks/useGithub";
import { useSettings } from "../../hooks/useSettings";
import { RowLabel } from "../settings/fields";
import { Trans, useTranslation } from "react-i18next";

/** Repos are stored as clone URLs but chosen as `owner/name`, the same way a
 * session draft does it. One prefix owns both directions. */
const GITHUB_PREFIX = "https://github.com/";
const repoUrl = (fullName: string) => `${GITHUB_PREFIX}${fullName}`;
const repoLabel = (url: string) =>
  url.startsWith(GITHUB_PREFIX) ? url.slice(GITHUB_PREFIX.length) : url;

/** Create (`/environments/new`) and edit (`/environments/:name/edit`) share one
 * form, mounted only once the environment has loaded: the rows seed from
 * `initial` with `useState`, which cannot pick up a value that arrives later. */
export function EnvironmentEditPage() {
  const { t } = useTranslation();
  const { name } = useParams<{ name: string }>();
  const { data: existing, isLoading, isError } = useEnvironment(name);

  if (name && isLoading) {
    return <p className="px-6 py-4 text-sm text-faint">{t("common.loading")}</p>;
  }
  if (name && (isError || !existing)) {
    return (
      <p className="px-6 py-4 text-sm text-red-ink">
{t("environmentEdit.noSuch", { name })}
      </p>
    );
  }
  return <EnvironmentForm key={name ?? "new"} initial={existing} />;
}

function EnvironmentForm({ initial }: { initial?: EnvironmentView }) {
  const { t } = useTranslation();
  const editing = !!initial;
  const create = useCreateEnvironment();
  const update = useUpdateEnvironment();
  const navigate = useNavigate();
  const [envName, setEnvName] = useState(initial?.name ?? "");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [vendor, setVendor] = useState(initial?.vendor ?? "");
  const [repos, setRepos] = useState<RepoConfig[]>(initial?.repos ?? []);
  const [envVars, setEnvVars] = useState<EnvVar[]>(initial?.envVars ?? []);
  const [provisionText, setProvisionText] = useState(
    initial?.provision.length ? JSON.stringify(initial.provision, null, 2) : "",
  );
  const [error, setError] = useState<string | null>(null);
  const busy = create.isPending || update.isPending;

  const { data: settings } = useSettings();
  // An environment provisions its own workspace, so only a vendor that can do
  // that can host one — the same rule the model file states, now enforced by
  // what the control offers instead of by what the user happens to type.
  const connected = useMemo(
    () =>
      (settings?.vendors ?? [])
        .filter((v) => v.capabilities.supportsProvisioning)
        .map((v) => v.name),
    [settings],
  );
  // A saved environment can name a vendor that has since gone away. Dropping
  // it from the list would silently rewrite the environment on the next save,
  // so it stays selectable and says why it is odd.
  const vendorOptions = useMemo(
    () =>
      initial?.vendor && !connected.includes(initial.vendor)
        ? [initial.vendor, ...connected]
        : connected,
    [connected, initial?.vendor],
  );

  const blockedReason =
    envName.trim() === ""
      ? t("environmentEdit.needName")
      : vendor.trim() === ""
        ? t("environmentEdit.needVendor")
        : null;
  const canSave = !busy && blockedReason === null;

  const handleSave = async () => {
    setError(null);
    let provision: ProvisionStep[] | undefined;
    const text = provisionText.trim();
    if (text) {
      try {
        const parsed: unknown = JSON.parse(text);
        if (!Array.isArray(parsed)) throw new Error("not an array");
        provision = parsed as ProvisionStep[];
      } catch {
        setError(t("environmentEdit.provisionInvalid"));
        return;
      }
    }
    const body = {
      name: envName.trim(),
      description: description.trim() || undefined,
      vendor: vendor.trim(),
      repos: repos.length ? repos : undefined,
      envVars: envVars.length ? envVars : undefined,
      provision,
    };
    try {
      if (editing) await update.mutateAsync({ name: envName.trim(), body });
      else await create.mutateAsync(body);
      navigate("/environments");
    } catch (e) {
      setError(
        e instanceof ApiRequestError
          ? e.message
          : t("environmentEdit.saveFailed"),
      );
    }
  };

  return (
    <div className="flex h-full flex-col" data-testid="environment-edit-page">
      <header className="flex h-[var(--header-h)] shrink-0 items-center bar-scroll gap-2 bg-panel px-4 sm:px-6">
        <RailToggle />
        <h1 className="page-title min-w-0 flex-1 truncate">
          {editing
            ? t("agentEdit.editTitle", { name: initial.name })
            : t("environments.new")}
        </h1>
        <button
          className="key key-blank"
          onClick={() => navigate("/environments")}
          data-testid="cancel-environment-button"
        >
          {t("common.cancel")}
        </button>
        <button
          className="key key-go"
          disabled={!canSave}
          onClick={handleSave}
          data-testid="save-environment-button"
        >
          {busy ? "Saving…" : "Save"}
        </button>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto" data-popover-boundary>
        <div className="mx-auto w-full max-w-3xl space-y-6 px-4 py-6 sm:px-6">
          <section className="section space-y-4">
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <label className="block">
                <RowLabel>{t("memoryPage.name")}</RowLabel>
                <input
                  className="field field-mono"
                  placeholder={t("environmentEdit.namePlaceholder")}
                  value={envName}
                  disabled={editing}
                  onChange={(e) => setEnvName(e.target.value)}
                  data-testid="environment-name-input"
                />
              </label>
              <label className="block">
                <RowLabel>{t("memoryPage.description")}</RowLabel>
                <input
                  className="field"
                  placeholder={t("environmentEdit.descriptionPlaceholder")}
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                  data-testid="environment-description-input"
                />
              </label>
            </div>
            <label className="block">
              <RowLabel>{t("environmentEdit.vendor")}</RowLabel>
              <select
                className="field font-mono"
                value={vendor}
                disabled={vendorOptions.length === 0}
                onChange={(e) => setVendor(e.target.value)}
                data-testid="environment-vendor-input"
              >
                <option value="">{t("environmentEdit.selectVendor")}</option>
                {vendorOptions.map((v) => (
                  <option key={v} value={v}>
                    {connected.includes(v)
                      ? v
                      : t("environmentEdit.vendorNotConnected", { name: v })}
                  </option>
                ))}
              </select>
              {vendorOptions.length === 0 && (
                <p className="mt-1 text-xs leading-relaxed text-faint">
                  <Trans
                    i18nKey="environmentEdit.noProvisioningVendor"
                    components={{
                      lnk: (
                        <Link
                          to="/settings/runtimes"
                          className="text-legend underline underline-offset-2"
                        />
                      ),
                    }}
                  />
                </p>
              )}
            </label>
          </section>

          <RepoPicker repos={repos} setRepos={setRepos} />

          <section className="section space-y-3">
            <h2 className="section-title">{t("environmentEdit.envVars")}</h2>
            <p className="text-xs text-faint">
{t("environmentEdit.envVarsHint")}
            </p>
            {envVars.map((v, i) => (
              <div key={i} className="flex items-center gap-2">
                <input
                  className="field field-mono w-56"
                  placeholder={t("environmentEdit.envVarName")}
                  value={v.name}
                  onChange={(e) =>
                    setEnvVars(
                      envVars.map((x, j) =>
                        j === i ? { ...x, name: e.target.value } : x,
                      ),
                    )
                  }
                  data-testid={`env-name-${i}`}
                />
                <input
                  className="field field-mono flex-1"
                  placeholder={t("environmentEdit.envVarValue")}
                  value={v.value}
                  onChange={(e) =>
                    setEnvVars(
                      envVars.map((x, j) =>
                        j === i ? { ...x, value: e.target.value } : x,
                      ),
                    )
                  }
                  data-testid={`env-value-${i}`}
                />
                <button
                  className="key-icon !h-7 !w-7 hover:!bg-red-quiet hover:!text-red-ink"
                  title={t("environmentEdit.removeEnvVar")}
                  data-testid={`env-remove-${i}`}
                  onClick={() => setEnvVars(envVars.filter((_, j) => j !== i))}
                >
                  <Trash2 size={15} />
                </button>
              </div>
            ))}
            <button
              className="key key-blank"
              onClick={() => setEnvVars([...envVars, { name: "", value: "" }])}
              data-testid="env-add"
            >
              <Plus size={13} aria-hidden />
              {t("environmentEdit.addEnvVar")}
            </button>
          </section>

          <section className="section space-y-3">
            <h2 className="section-title">{t("environmentEdit.provision")}</h2>
            <p className="text-xs text-faint">
{t("environmentEdit.provisionHint")}
            </p>
            <textarea
              className="field field-mono min-h-28 w-full"
              placeholder='[{"name": "setup", "uses": "run", "with": [{"key": "cmd", "value": "make setup"}]}]'
              value={provisionText}
              onChange={(e) => setProvisionText(e.target.value)}
              data-testid="provision-input"
            />
          </section>

          {error && (
            <div
              className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink"
              data-testid="environment-error"
            >
              {error}
            </div>
          )}

          {blockedReason && (
            <p
              className="text-xs leading-relaxed text-dim"
              data-testid="environment-blocked-hint"
            >
              {blockedReason}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * Repos are picked from the GitHub App installation, not typed.
 *
 * A clone URL is a thing you get wrong silently — a typo saves fine and fails
 * at provision time, hours later on someone else's machine. Sessions already
 * pick from the installation for exactly that reason; this is the same list in
 * the shape a full page can afford, with the ref beside each repo it belongs
 * to rather than in a parallel column.
 */
function RepoPicker({
  repos,
  setRepos,
}: {
  repos: RepoConfig[];
  setRepos: (next: RepoConfig[]) => void;
}) {
  const { t } = useTranslation();
  const { data: status } = useGithubStatus();
  const connected = !!status?.connected;
  const {
    data: repoList,
    isLoading,
    isError,
    error: loadError,
  } = useGithubRepos(connected);
  const [filter, setFilter] = useState("");

  const chosen = new Map(repos.map((r) => [r.url, r.gitRef ?? ""]));
  const listed = repoList?.repos ?? [];
  // A repo can be selected and yet absent from the listing — the installation
  // changed, or the URL predates this picker. It still renders, so unchecking
  // it is a decision rather than a side effect of saving.
  const unlisted = repos
    .filter((r) => !listed.some((l) => repoUrl(l.fullName) === r.url))
    .map((r) => ({ url: r.url, fullName: repoLabel(r.url), missing: true }));
  const rows = [
    ...unlisted,
    ...listed.map((l) => ({
      url: repoUrl(l.fullName),
      fullName: l.fullName,
      missing: false,
    })),
  ];
  const needle = filter.trim().toLowerCase();
  const visible = needle
    ? rows.filter((r) => r.fullName.toLowerCase().includes(needle))
    : rows;

  const toggle = (url: string, checked: boolean) =>
    setRepos(
      checked ? repos.filter((r) => r.url !== url) : [...repos, { url }],
    );
  const setRef = (url: string, gitRef: string) =>
    setRepos(
      repos.map((r) => (r.url === url ? { ...r, gitRef: gitRef || undefined } : r)),
    );

  return (
    <section className="section space-y-3">
      <div className="flex items-baseline justify-between gap-4">
        <h2 className="section-title">{t("environment.repos")}</h2>
        <span className="legend" data-testid="repo-selected-count">
{t("channel.selectedCount", { count: repos.length })}
        </span>
      </div>
      {/* Above the chain rather than inside it: the repos already ticked still
          render below, so a failed listing does not take away the only control
          that can untick one. */}
      {connected && isError && (
        <ReadError
          what={t("environment.reposLower")}
          error={loadError}
          testId="repo-list-error"
        />
      )}

      {!connected ? (
        <p
          className="screen px-3 py-5 text-center text-sm leading-relaxed text-faint"
          data-testid="repo-github-prompt"
        >
          <Trans
            i18nKey="environmentEdit.reposFromGithub"
            components={{
              lnk: (
                <Link
                  to="/settings/integrations"
                  className="text-legend underline underline-offset-2"
                />
              ),
            }}
          />
        </p>
      ) : isLoading && rows.length === 0 ? (
        <p className="screen px-3 py-5 text-center text-sm text-faint">
{t("environmentEdit.loadingRepos")}
        </p>
      ) : rows.length === 0 ? (
        // A failed listing has already said so above; "no repos are visible to
        // the app installation" would name the wrong cause.
        isError ? null : (
          <p className="screen px-3 py-5 text-center text-sm leading-relaxed text-faint">
            <Trans
              i18nKey="environmentEdit.noReposVisible"
              components={{
                lnk: (
                  <Link
                    to="/settings/integrations"
                    className="text-legend underline underline-offset-2"
                  />
                ),
              }}
            />
          </p>
        )
      ) : (
        <>
          <input
            className="field field-mono"
            placeholder={t("environmentEdit.filterRepos")}
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            data-testid="repo-filter"
          />
          <div className="max-h-72 space-y-0.5 overflow-y-auto pr-0.5">
            {visible.map((r) => {
              const checked = chosen.has(r.url);
              return (
                <div
                  key={r.url}
                  className="flex items-center gap-2 rounded-[var(--radius-chip)] px-1.5 py-1 hover:bg-raised"
                >
                  <label className="flex min-w-0 flex-1 cursor-pointer items-center gap-2">
                    <input
                      type="checkbox"
                      checked={checked}
                      onChange={() => toggle(r.url, checked)}
                      data-testid={`repo-toggle-${r.fullName}`}
                    />
                    <span className="min-w-0 flex-1 truncate font-mono text-[0.8125rem]">
                      {r.fullName}
                    </span>
                    {r.missing && (
                      <span className="chip shrink-0">
                        {t("environmentEdit.notInInstallation")}
                      </span>
                    )}
                  </label>
                  {checked && (
                    // Shallower than a standalone field: a checked row is
                    // otherwise twice the height of an unchecked one, and the
                    // list reads as ragged rather than as one column.
                    <input
                      className="field field-mono w-32 shrink-0 !py-1"
                      placeholder={t("environmentEdit.ref")}
                      value={chosen.get(r.url) ?? ""}
                      onChange={(e) => setRef(r.url, e.target.value)}
                      data-testid={`repo-ref-${r.fullName}`}
                      aria-label={t("environmentEdit.gitRefFor", { name: r.fullName })}
                    />
                  )}
                </div>
              );
            })}
            {visible.length === 0 && (
              <p className="px-1.5 py-3 text-sm text-faint">
{t("environmentEdit.noRepoMatches", { query: filter.trim() })}
              </p>
            )}
          </div>
        </>
      )}
    </section>
  );
}
