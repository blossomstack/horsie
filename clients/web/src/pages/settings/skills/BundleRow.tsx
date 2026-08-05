import { Loader2, RotateCcw, Trash2, Webhook } from "lucide-react";
import type { PluginView } from "../../../api/types";
import { cn } from "../../../lib/cn";
import {
  useRemovePlugin,
  useSetPluginDefault,
  useUpdatePlugin,
} from "../../../hooks/usePlugins";

export function BundleRow({ bundle }: { bundle: PluginView }) {
  const setDefault = useSetPluginDefault();
  const update = useUpdatePlugin();
  const remove = useRemovePlugin();

  return (
    <div
      className="rounded-[var(--radius-control)] border p-3"
      style={{ background: "var(--panel-raised)" }}
      data-testid="bundle-row"
    >
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="item-title truncate">{bundle.name}</span>
            {bundle.version && (
              <span className="chip !py-0 text-[0.625rem]">{bundle.version}</span>
            )}
            {/* Where it came from, so a bundle installed through a catalogue is
                not indistinguishable from one pasted by URL. */}
            {bundle.marketplace && (
              <span className="chip !py-0 text-[0.625rem]">
                {bundle.marketplace}
              </span>
            )}
            {bundle.hasHooks && (
              <span className="chip !py-0 flex items-center gap-1 text-[0.625rem]">
                <Webhook size={11} /> hooks
              </span>
            )}
          </div>
          {bundle.description && (
            <p className="mt-0.5 text-xs text-dim">{bundle.description}</p>
          )}
          <p className="mt-0.5 text-[0.6875rem] text-faint">
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
            className="key shrink-0 !px-2.5 !py-1.5 text-xs"
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
            className="key-icon shrink-0 text-faint hover:text-red-ink"
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
        checked ? "bg-orange" : "bg-raised",
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
