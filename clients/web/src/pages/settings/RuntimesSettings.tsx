import { Loader2 } from "lucide-react";
import { useEffect, useState } from "react";
import { ApiRequestError } from "../../api/client";
import { cn } from "../../lib/cn";
import { useSettings, useUpdateSettings } from "../../hooks/useSettings";
import { RowLabel } from "./fields";
import { SettingsHeader } from "./SettingsHeader";
import { usePublishDirty } from "./dirty";

/**
 * Where sessions execute.
 *
 * There is nothing to configure here any more: every vendor is an external
 * agent process that dials the server and announces itself, so this page is a
 * live roster plus the one thing the server does own — which vendor new
 * sessions default to. A vendor's own settings (a velos URL and token, the
 * directories a laptop agent serves) live in that agent's configuration,
 * because that is the process that holds them.
 */
export function RuntimesSettings() {
  const { data: settings, isLoading, error } = useSettings();
  const update = useUpdateSettings();
  const [defaultVendor, setDefaultVendor] = useState("");
  const [saveError, setSaveError] = useState<string | null>(null);

  const dirty = settings != null && defaultVendor !== settings.defaultVendor;
  usePublishDirty(dirty);

  useEffect(() => {
    if (settings) setDefaultVendor(settings.defaultVendor);
  }, [settings]);

  const save = async () => {
    setSaveError(null);
    try {
      await update.mutateAsync({ defaultVendor: defaultVendor || undefined });
    } catch (e) {
      setSaveError(
        e instanceof ApiRequestError ? e.message : "failed to save settings",
      );
    }
  };

  if (isLoading) {
    return (
      <div className="flex items-center gap-2 p-6 text-sm text-neutral-500">
        <Loader2 className="h-4 w-4 animate-spin" />
        Loading runtimes…
      </div>
    );
  }
  if (error || !settings) {
    return (
      <div className="p-6 text-sm text-red-600">Could not load settings.</div>
    );
  }

  const vendors = settings.vendors;

  return (
    <div className="flex flex-col">
      <SettingsHeader
        title="Runtimes"
        desc="Where sessions execute. Vendors are agent processes that connect to this server; each one is configured where it runs."
        dirty={dirty}
        saving={update.isPending}
        onSave={save}
        onDiscard={() => setDefaultVendor(settings.defaultVendor)}
      />
      {saveError && (
        <div className="mx-6 mt-4 rounded border border-red-300 bg-red-50 p-3 text-sm text-red-700">
          {saveError}
        </div>
      )}

      <section className="card p-4">
        <h2 className="text-sm font-semibold text-text">Connected vendors</h2>
        <p className="mt-0.5 mb-3 text-xs text-faint">
          Agents connected right now. Run <code>horsie connect</code> on a
          machine, or start a vendor agent such as{" "}
          <code>horsie-velos-runtime</code>, and it appears here.
        </p>
        {vendors.length === 0 ? (
          <p className="rounded-[var(--radius)] border border-dashed px-3 py-4 text-center text-sm text-faint">
            No vendor agents are connected, so sessions cannot run a turn yet.
          </p>
        ) : (
          <ul className="divide-y">
            {vendors.map((v) => (
              <li
                key={v.name}
                className="flex items-center justify-between py-2.5"
              >
                <div className="flex items-center gap-2.5">
                  <span className="font-mono text-sm text-text">{v.name}</span>
                  {v.isDefault && (
                    <span className="rounded-[var(--radius)] border px-1.5 py-0.5 text-[11px] text-muted">
                      default
                    </span>
                  )}
                </div>
                <span className="text-xs text-faint">
                  {v.capabilities.supportsProvisioning
                    ? "provisions repos and skill bundles"
                    : "works in the agent's own directories"}
                </span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="card mt-4 p-4">
        <h2 className="text-sm font-semibold text-text">Default vendor</h2>
        <p className="mt-0.5 mb-3 text-xs text-faint">
          Which vendor new sessions use when they don't pick one. It may name an
          agent that isn't connected yet — the preference applies once that
          agent dials in.
        </p>
        <label className="block max-w-sm">
          <RowLabel>Vendor name</RowLabel>
          <input
            className="input w-full"
            value={defaultVendor}
            placeholder="local"
            onChange={(e) => setDefaultVendor(e.target.value)}
          />
        </label>
        {defaultVendor !== "" &&
          !vendors.some((v) => v.name === defaultVendor) && (
            <p className={cn("mt-2 text-xs text-faint")}>
              No connected vendor is named "{defaultVendor}" right now. Sessions
              defaulting to it will fail to start until its agent connects.
            </p>
          )}
      </section>
    </div>
  );
}
