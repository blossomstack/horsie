import * as Dialog from "@radix-ui/react-dialog";
import { ChevronRight, Loader2, Settings2, X } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { ApiRequestError } from "../api/client";
import type { CreateSessionRequest } from "../api/types";
import { cn } from "../lib/cn";
import { useCreateSession } from "../hooks/useSessions";
import { useSettings } from "../hooks/useSettings";

export function NewSessionModal({
  open,
  onOpenChange,
  onCreated,
}: {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  onCreated: (id: string) => void;
}) {
  const create = useCreateSession();
  const { data: settings } = useSettings();
  const models = settings?.models ?? [];
  const activeVendors = (settings?.vendors ?? []).filter((v) => v.active);
  const showVendor = activeVendors.length > 1;

  const [name, setName] = useState("");
  const [model, setModel] = useState("");
  const [workdir, setWorkdir] = useState("");
  const [vendor, setVendor] = useState("");
  const [systemPrompt, setSystemPrompt] = useState("");
  const [allowAskUser, setAllowAskUser] = useState(true);
  const [usePlugins, setUsePlugins] = useState(false);
  const [advanced, setAdvanced] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reset = () => {
    setName("");
    setModel("");
    setWorkdir("");
    setVendor("");
    setSystemPrompt("");
    setAllowAskUser(true);
    setUsePlugins(false);
    setAdvanced(false);
    setError(null);
  };

  // Clear the form on close so a cancelled draft never carries into the next
  // open (and a since-deleted model can't linger as a stale selection).
  useEffect(() => {
    if (!open) reset();
  }, [open]);

  // While open, keep model/vendor pointing at a choice that still exists —
  // reseed from server config when the current selection is empty or stale.
  useEffect(() => {
    if (!open || !settings) return;
    if (!models.some((m) => m.alias === model))
      setModel(models[0]?.alias ?? "");
    if (!activeVendors.some((v) => v.name === vendor))
      setVendor(settings.defaultVendor);
  }, [open, models, activeVendors, settings, model, vendor]);

  const submit = async () => {
    setError(null);
    const wd = workdir.trim();
    if (!model.trim()) return setError("Select a model.");
    if (!wd) return setError("A workspace directory is required.");

    const body: CreateSessionRequest = {
      name: name.trim() || undefined,
      agent: {
        model: model.trim(),
        systemPrompt: systemPrompt.trim() || undefined,
        allowAskUser,
        usePlugins,
      },
      workdirs: [wd],
      vendor: vendor.trim() || undefined,
    };

    try {
      const res = await create.mutateAsync(body);
      reset();
      onOpenChange(false);
      onCreated(res.session.id);
    } catch (e) {
      setError(
        e instanceof ApiRequestError ? e.message : "Failed to create session.",
      );
    }
  };

  const noModels = !!settings && models.length === 0;

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/50 backdrop-blur-sm animate-fade-in" />
        <Dialog.Content
          className="fixed left-1/2 top-1/2 z-50 w-[min(30rem,calc(100vw-2rem))] -translate-x-1/2 -translate-y-1/2 rounded-[var(--radius-xl)] border p-5 shadow-2xl animate-rise focus:outline-none"
          style={{ background: "var(--surface)" }}
        >
          <div className="mb-4 flex items-center justify-between">
            <Dialog.Title className="text-base font-semibold text-text">
              New session
            </Dialog.Title>
            <Dialog.Close className="btn-icon" aria-label="Close">
              <X size={18} />
            </Dialog.Close>
          </div>

          <div className="space-y-3.5">
            <Field label="Name" hint="optional">
              <input
                className="input"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="Untitled session"
              />
            </Field>

            <Field label="Model">
              {noModels ? (
                <Link
                  to="/settings"
                  onClick={() => onOpenChange(false)}
                  className="flex items-center gap-1.5 rounded-[var(--radius)] border border-dashed px-3 py-2 text-sm text-muted transition-colors hover:text-text"
                >
                  <Settings2 size={14} />
                  No models configured — add one in Settings
                </Link>
              ) : (
                <select
                  className="input font-mono"
                  value={model}
                  onChange={(e) => setModel(e.target.value)}
                >
                  {models.map((m) => (
                    <option key={m.alias} value={m.alias}>
                      {m.alias} — {m.modelId}
                    </option>
                  ))}
                </select>
              )}
            </Field>

            <Field
              label="Workspace directory"
              hint="absolute path the agent can work in"
            >
              <input
                className="input font-mono"
                value={workdir}
                onChange={(e) => setWorkdir(e.target.value)}
                placeholder="/Users/you/project"
                autoFocus
              />
            </Field>

            <button
              className="flex items-center gap-1 text-xs font-medium text-muted transition-colors hover:text-text"
              onClick={() => setAdvanced((a) => !a)}
            >
              <ChevronRight
                size={13}
                className={cn("transition-transform", advanced && "rotate-90")}
              />
              Advanced
            </button>

            {advanced && (
              <div className="space-y-3.5 border-t pt-3.5">
                {showVendor && (
                  <Field label="Runtime vendor">
                    <select
                      className="input font-mono"
                      value={vendor}
                      onChange={(e) => setVendor(e.target.value)}
                    >
                      {activeVendors.map((v) => (
                        <option key={v.name} value={v.name}>
                          {v.name}
                          {v.isDefault ? " (default)" : ""}
                        </option>
                      ))}
                    </select>
                  </Field>
                )}
                <Field label="System prompt" hint="optional">
                  <textarea
                    className="input min-h-[68px] resize-y"
                    value={systemPrompt}
                    onChange={(e) => setSystemPrompt(e.target.value)}
                    placeholder="Override the default system prompt…"
                  />
                </Field>
                <Toggle
                  label="Allow the agent to ask you questions"
                  checked={allowAskUser}
                  onChange={setAllowAskUser}
                />
                <Toggle
                  label="Enable plugins"
                  checked={usePlugins}
                  onChange={setUsePlugins}
                />
              </div>
            )}

            {error && (
              <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
                {error}
              </div>
            )}
          </div>

          <div className="mt-5 flex justify-end gap-2">
            <Dialog.Close className="btn-ghost">Cancel</Dialog.Close>
            <button
              className="btn-primary"
              onClick={submit}
              disabled={create.isPending || noModels}
            >
              {create.isPending && (
                <Loader2 size={15} className="animate-spin" />
              )}
              Create session
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: ReactNode;
}) {
  return (
    <label className="block">
      <div className="mb-1 flex items-baseline gap-2">
        <span className="text-xs font-semibold text-text">{label}</span>
        {hint && <span className="text-[11px] text-faint">{hint}</span>}
      </div>
      {children}
    </label>
  );
}

function Toggle({
  label,
  checked,
  onChange,
}: {
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <label className="flex cursor-pointer items-center gap-2.5 text-sm text-text">
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        onClick={() => onChange(!checked)}
        className={cn(
          "relative h-5 w-9 shrink-0 rounded-full transition-colors",
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
      {label}
    </label>
  );
}
