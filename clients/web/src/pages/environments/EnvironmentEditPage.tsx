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
  const { name } = useParams<{ name: string }>();
  const { data: existing, isLoading, isError } = useEnvironment(name);

  if (name && isLoading) {
    return <p className="px-6 py-4 text-sm text-faint">Loading…</p>;
  }
  if (name && (isError || !existing)) {
    return (
      <p className="px-6 py-4 text-sm text-red-ink">
        No such environment: {name}.
      </p>
    );
  }
  return <EnvironmentForm key={name ?? "new"} initial={existing} />;
}

function EnvironmentForm({ initial }: { initial?: EnvironmentView }) {
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
      ? "Give the environment a name to save it."
      : vendor.trim() === ""
        ? "Choose the runtime vendor this environment runs on."
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
        setError("Provision steps must be a JSON array of {name, uses, with}.");
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
        e instanceof ApiRequestError ? e.message : "Failed to save environment.",
      );
    }
  };

  return (
    <div className="flex h-full flex-col" data-testid="environment-edit-page">
      <header className="flex h-[3.25rem] shrink-0 items-center gap-2 border-b bg-panel px-4 sm:px-6">
        <RailToggle />
        <h1 className="page-title min-w-0 flex-1 truncate">
          {editing ? `Edit ${initial.name}` : "New environment"}
        </h1>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto" data-popover-boundary>
        <div className="mx-auto w-full max-w-3xl space-y-6 px-4 py-6 sm:px-6">
          <section className="panel space-y-4 p-4">
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <label className="block">
                <RowLabel>Name</RowLabel>
                <input
                  className="field field-mono"
                  placeholder="staging"
                  value={envName}
                  disabled={editing}
                  onChange={(e) => setEnvName(e.target.value)}
                  data-testid="environment-name-input"
                />
              </label>
              <label className="block">
                <RowLabel>Description</RowLabel>
                <input
                  className="field"
                  placeholder="What this environment is for"
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                  data-testid="environment-description-input"
                />
              </label>
            </div>
            <label className="block">
              <RowLabel>Runtime vendor</RowLabel>
              <select
                className="field font-mono"
                value={vendor}
                disabled={vendorOptions.length === 0}
                onChange={(e) => setVendor(e.target.value)}
                data-testid="environment-vendor-input"
              >
                <option value="">Select a runtime vendor</option>
                {vendorOptions.map((v) => (
                  <option key={v} value={v}>
                    {connected.includes(v) ? v : `${v} — not connected`}
                  </option>
                ))}
              </select>
              <p className="mt-1 text-xs leading-relaxed text-faint">
                {vendorOptions.length === 0 ? (
                  <>
                    No connected vendor provisions its own workspace, so nothing
                    can run an environment yet. Add one under{" "}
                    <Link
                      to="/settings/runtimes"
                      className="text-legend underline underline-offset-2"
                    >
                      Settings › Runtimes
                    </Link>
                    .
                  </>
                ) : (
                  "Only vendors that provision their own workspace can run an environment, so local runtimes are not listed."
                )}
              </p>
            </label>
          </section>

          <RepoPicker repos={repos} setRepos={setRepos} />

          <section className="panel space-y-3 p-4">
            <h2 className="section-title">Env vars</h2>
            <p className="text-xs text-faint">
              Plain text only — no secrets here.
            </p>
            {envVars.map((v, i) => (
              <div key={i} className="flex items-center gap-2">
                <input
                  className="field field-mono w-56"
                  placeholder="NAME"
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
                  placeholder="value"
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
                  title="Remove env var"
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
              Add env var
            </button>
          </section>

          <section className="panel space-y-3 p-4">
            <h2 className="section-title">Provision steps</h2>
            <p className="text-xs text-faint">
              A JSON array of {"{name, uses, with}"} steps. Nothing runs them
              yet.
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

          <div className="flex flex-wrap items-center gap-2">
            <button
              className="key key-go"
              disabled={!canSave}
              onClick={handleSave}
              data-testid="save-environment-button"
            >
              {busy ? "Saving…" : "Save environment"}
            </button>
            <button
              className="key key-blank"
              onClick={() => navigate("/environments")}
            >
              Cancel
            </button>
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
    <section className="panel space-y-3 p-4">
      <div className="flex items-baseline justify-between gap-4">
        <h2 className="section-title">Repos</h2>
        <span className="legend" data-testid="repo-selected-count">
          {repos.length} selected
        </span>
      </div>
      <p className="text-xs leading-relaxed text-faint">
        Cloned into the runtime workspace at provision time. Leave a ref blank
        to take the repo's default branch.
      </p>

      {/* Above the chain rather than inside it: the repos already ticked still
          render below, so a failed listing does not take away the only control
          that can untick one. */}
      {connected && isError && (
        <ReadError what="repos" error={loadError} testId="repo-list-error" />
      )}

      {!connected ? (
        <p
          className="screen px-3 py-5 text-center text-sm leading-relaxed text-faint"
          data-testid="repo-github-prompt"
        >
          Repos come from your GitHub App installation.{" "}
          <Link
            to="/settings/integrations"
            className="text-legend underline underline-offset-2"
          >
            Connect GitHub
          </Link>{" "}
          to pick them.
        </p>
      ) : isLoading && rows.length === 0 ? (
        <p className="screen px-3 py-5 text-center text-sm text-faint">
          Loading repos…
        </p>
      ) : rows.length === 0 ? (
        // A failed listing has already said so above; "no repos are visible to
        // the app installation" would name the wrong cause.
        isError ? null : (
          <p className="screen px-3 py-5 text-center text-sm leading-relaxed text-faint">
            No repos are visible to the app installation.{" "}
            <Link
              to="/settings/integrations"
              className="text-legend underline underline-offset-2"
            >
              Check its access
            </Link>
            .
          </p>
        )
      ) : (
        <>
          <input
            className="field field-mono"
            placeholder="Filter repos"
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
                      <span className="chip shrink-0">not in installation</span>
                    )}
                  </label>
                  {checked && (
                    // Shallower than a standalone field: a checked row is
                    // otherwise twice the height of an unchecked one, and the
                    // list reads as ragged rather than as one column.
                    <input
                      className="field field-mono w-32 shrink-0 !py-1"
                      placeholder="ref"
                      value={chosen.get(r.url) ?? ""}
                      onChange={(e) => setRef(r.url, e.target.value)}
                      data-testid={`repo-ref-${r.fullName}`}
                      aria-label={`Git ref for ${r.fullName}`}
                    />
                  )}
                </div>
              );
            })}
            {visible.length === 0 && (
              <p className="px-1.5 py-3 text-sm text-faint">
                No repo matches “{filter.trim()}”.
              </p>
            )}
          </div>
        </>
      )}
    </section>
  );
}
