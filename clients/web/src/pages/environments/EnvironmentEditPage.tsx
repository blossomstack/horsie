import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { Plus, Trash2 } from "lucide-react";
import { ApiRequestError } from "../../api/client";
import { RailToggle } from "../../components/rail";
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
import { RowLabel } from "../settings/fields";

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
  const blockedReason =
    envName.trim() === ""
      ? "Give the environment a name to save it."
      : vendor.trim() === ""
        ? "Name the runtime vendor this environment runs on."
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
              <input
                className="field field-mono"
                placeholder="fly"
                value={vendor}
                onChange={(e) => setVendor(e.target.value)}
                data-testid="environment-vendor-input"
              />
              <p className="mt-1 text-xs text-faint">
                The vendor that runs this environment. Local runtimes are not
                supported.
              </p>
            </label>
          </section>

          <section className="panel space-y-3 p-4">
            <h2 className="section-title">Repos</h2>
            {repos.map((r, i) => (
              <div key={i} className="flex items-center gap-2">
                <input
                  className="field field-mono flex-1"
                  placeholder="https://github.com/org/repo"
                  value={r.url}
                  onChange={(e) =>
                    setRepos(
                      repos.map((x, j) =>
                        j === i ? { ...x, url: e.target.value } : x,
                      ),
                    )
                  }
                  data-testid={`repo-url-${i}`}
                />
                <input
                  className="field field-mono w-32"
                  placeholder="ref"
                  value={r.gitRef ?? ""}
                  onChange={(e) =>
                    setRepos(
                      repos.map((x, j) =>
                        j === i
                          ? { ...x, gitRef: e.target.value || undefined }
                          : x,
                      ),
                    )
                  }
                  data-testid={`repo-ref-${i}`}
                />
                <button
                  className="key-icon !h-7 !w-7 hover:!bg-red-quiet hover:!text-red-ink"
                  title="Remove repo"
                  data-testid={`repo-remove-${i}`}
                  onClick={() => setRepos(repos.filter((_, j) => j !== i))}
                >
                  <Trash2 size={15} />
                </button>
              </div>
            ))}
            <button
              className="key key-blank"
              onClick={() => setRepos([...repos, { url: "" }])}
              data-testid="repo-add"
            >
              <Plus size={13} aria-hidden />
              Add repo
            </button>
          </section>

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
