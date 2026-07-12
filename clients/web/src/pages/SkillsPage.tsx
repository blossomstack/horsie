import {
  Boxes,
  Download,
  Loader2,
  RotateCcw,
  Trash2,
  Webhook,
} from "lucide-react";
import { useState, type ReactNode } from "react";
import { ApiRequestError } from "../api/client";
import type { PluginView } from "../api/types";
import { cn } from "../lib/cn";
import {
  useInstallPlugin,
  usePlugins,
  useRemovePlugin,
  useSetPluginDefault,
  useUpdatePlugin,
} from "../hooks/usePlugins";

export function SkillsPage() {
  const { data: bundles, isLoading, isError } = usePlugins();
  const install = useInstallPlugin();

  const [sourceUrl, setSourceUrl] = useState("");
  const [sourceRef, setSourceRef] = useState("");

  const submitInstall = async () => {
    const url = sourceUrl.trim();
    if (!url) return;
    try {
      await install.mutateAsync({
        sourceUrl: url,
        sourceRef: sourceRef.trim() || undefined,
      });
      setSourceUrl("");
      setSourceRef("");
    } catch {
      /* surfaced from install.error below */
    }
  };

  const installError =
    install.error instanceof ApiRequestError
      ? install.error.message
      : install.isError
        ? "Failed to install bundle."
        : null;

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <header className="flex items-center gap-3 border-b px-6 py-3.5">
        <div>
          <h1 className="text-[15px] font-semibold text-text">Skills</h1>
          <p className="text-xs text-faint">
            Shareable skill bundles installed from git repos — pick them per
            session.
          </p>
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-3xl space-y-6 px-6 py-6">
          <section className="card p-4">
            <div className="mb-3 flex items-start gap-2">
              <Download size={15} className="mt-0.5 text-faint" />
              <div>
                <h2 className="text-sm font-semibold text-text">
                  Install a skill bundle
                </h2>
                <p className="mt-0.5 text-xs text-faint">
                  Clone a public git repo of skills. This can take a few seconds.
                </p>
              </div>
            </div>

            <div className="grid grid-cols-[1fr_auto] gap-3">
              <TextField
                label="Git URL"
                value={sourceUrl}
                onChange={setSourceUrl}
                placeholder="https://github.com/owner/skills-bundle"
              />
              <TextField
                label="Ref (optional)"
                value={sourceRef}
                onChange={setSourceRef}
                placeholder="main"
              />
            </div>

            {installError && (
              <div className="mt-3 rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
                {installError}
              </div>
            )}

            <div className="mt-3 flex justify-end">
              <button
                className="btn-primary"
                onClick={submitInstall}
                disabled={!sourceUrl.trim() || install.isPending}
              >
                {install.isPending ? (
                  <Loader2 size={15} className="animate-spin" />
                ) : (
                  <Download size={15} />
                )}
                Install
              </button>
            </div>
          </section>

          <section className="card p-4">
            <div className="mb-3 flex items-start gap-2">
              <Boxes size={15} className="mt-0.5 text-faint" />
              <div>
                <h2 className="text-sm font-semibold text-text">
                  Installed bundles
                </h2>
                <p className="mt-0.5 text-xs text-faint">
                  Toggle a bundle on to pre-select it for new sessions.
                </p>
              </div>
            </div>

            <div className="space-y-2.5">
              {isLoading && (
                <p className="py-8 text-center text-sm text-faint">Loading…</p>
              )}
              {isError && (
                <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
                  Couldn’t load bundles. Is <code>horsie serve</code> running?
                </div>
              )}
              {bundles && bundles.length === 0 && (
                <p className="rounded-[var(--radius)] border border-dashed px-3 py-4 text-center text-sm text-faint">
                  No skill bundles installed yet.
                </p>
              )}
              {bundles?.map((b) => (
                <BundleRow key={b.name} bundle={b} />
              ))}
            </div>
          </section>
        </div>
      </div>
    </div>
  );
}

function BundleRow({ bundle }: { bundle: PluginView }) {
  const setDefault = useSetPluginDefault();
  const update = useUpdatePlugin();
  const remove = useRemovePlugin();

  return (
    <div
      className="rounded-[var(--radius)] border p-3"
      style={{ background: "var(--surface-2)" }}
    >
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate font-mono text-sm font-semibold text-text">
              {bundle.name}
            </span>
            {bundle.version && (
              <span className="chip !py-0 text-[10px]">{bundle.version}</span>
            )}
            {bundle.hasHooks && (
              <span className="chip !py-0 flex items-center gap-1 text-[10px]">
                <Webhook size={11} /> hooks
              </span>
            )}
          </div>
          {bundle.description && (
            <p className="mt-0.5 text-xs text-muted">{bundle.description}</p>
          )}
          <p className="mt-0.5 text-[11px] text-faint">
            {bundle.skillCount} skill{bundle.skillCount === 1 ? "" : "s"}
          </p>
        </div>

        <div className="flex shrink-0 items-center gap-2">
          <Toggle
            label="Default for new sessions"
            checked={bundle.enabledDefault}
            disabled={setDefault.isPending}
            onChange={() =>
              setDefault.mutate({
                name: bundle.name,
                enabledDefault: !bundle.enabledDefault,
              })
            }
          />
          <button
            className="btn-outline shrink-0 !px-2.5 !py-1.5 text-xs"
            onClick={() => update.mutate(bundle.name)}
            disabled={update.isPending}
          >
            {update.isPending ? (
              <Loader2 size={13} className="animate-spin" />
            ) : (
              <RotateCcw size={13} />
            )}
            Update
          </button>
          <button
            className="btn-icon shrink-0 text-faint hover:text-error"
            onClick={() => {
              if (confirm(`Delete skill bundle "${bundle.name}"?`))
                remove.mutate(bundle.name);
            }}
            disabled={remove.isPending}
            aria-label="Delete bundle"
          >
            <Trash2 size={15} />
          </button>
        </div>
      </div>
    </div>
  );
}

function RowLabel({ children }: { children: ReactNode }) {
  return (
    <span className="mb-1 block text-[11px] font-semibold text-muted">
      {children}
    </span>
  );
}

function TextField({
  label,
  value,
  onChange,
  placeholder,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
}) {
  return (
    <label className="block">
      <RowLabel>{label}</RowLabel>
      <input
        className="input font-mono"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
      />
    </label>
  );
}

function Toggle({
  label,
  checked,
  disabled,
  onChange,
}: {
  label: string;
  checked: boolean;
  disabled?: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={cn(
        "relative h-5 w-9 shrink-0 rounded-full transition-colors disabled:opacity-50",
        checked ? "bg-accent" : "bg-surface-3",
      )}
    >
      <span
        className={cn(
          "absolute top-0.5 h-4 w-4 rounded-full bg-white transition-transform",
          checked ? "translate-x-4" : "translate-x-0.5",
        )}
      />
    </button>
  );
}
