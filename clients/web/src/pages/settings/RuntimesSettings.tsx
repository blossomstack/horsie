import { useEffect, useState } from "react";
import { ApiRequestError } from "../../api/client";
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
        e instanceof ApiRequestError ? e.message : "Failed to save settings.",
      );
    }
  };

  if (isLoading) {
    return (
      <div className="flex items-center gap-2 p-6">
        <span className="lamp lamp-live text-amber-ink" aria-hidden />
        <span className="legend">Loading runtimes</span>
      </div>
    );
  }
  if (error || !settings) {
    return (
      <div className="p-6">
        <p className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
          Couldn’t load settings. Check that horsie-server is running, then
          reload.
        </p>
      </div>
    );
  }

  const vendors = settings.vendors;

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <SettingsHeader
        title="Runtimes"
        desc="Where sessions execute. Vendors are agent processes that connect to this server; each one is configured where it runs."
        dirty={dirty}
        saving={update.isPending}
        onSave={save}
        onDiscard={() => setDefaultVendor(settings.defaultVendor)}
      />

      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-3xl space-y-6 px-4 py-6 sm:px-6">
          {saveError && (
            <p className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
              {saveError}
            </p>
          )}

          <section className="panel p-4">
            <h2 className="font-mono text-[12px] font-semibold uppercase tracking-[0.1em] text-legend">
              Connected vendors
            </h2>
            <p className="mt-1.5 max-w-prose text-xs leading-relaxed text-faint">
              Agents connected right now. Run <code>horsie connect</code> on a
              machine, or start a vendor agent such as{" "}
              <code>horsie-velos-runtime</code>, and it appears here.
            </p>

            {vendors.length === 0 ? (
              <p className="screen mt-4 px-3 py-5 text-center text-sm text-faint">
                No vendor agents are connected, so sessions cannot run a turn
                yet.
              </p>
            ) : (
              <ul className="mt-4 space-y-px">
                {vendors.map((v) => (
                  <li
                    key={v.name}
                    className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-[var(--radius-control)] bg-raised px-3 py-2.5"
                  >
                    <span className="lamp text-lamp-ok" aria-hidden />
                    <span className="font-mono text-[13px] text-legend">
                      {v.name}
                    </span>
                    {v.isDefault && <span className="chip">Default</span>}
                    <span className="legend ml-auto">
                      {v.capabilities.supportsProvisioning
                        ? "Provisions repos and skill bundles"
                        : "Works in the agent’s own directories"}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </section>

          <section className="panel p-4">
            <h2 className="font-mono text-[12px] font-semibold uppercase tracking-[0.1em] text-legend">
              Default vendor
            </h2>
            <p className="mt-1.5 max-w-prose text-xs leading-relaxed text-faint">
              Which vendor new sessions use when they don’t pick one. It may name
              an agent that isn’t connected yet — the preference applies once
              that agent dials in.
            </p>
            <label className="mt-4 block max-w-sm">
              <RowLabel>Vendor name</RowLabel>
              <input
                className="field field-mono"
                value={defaultVendor}
                placeholder="local"
                onChange={(e) => setDefaultVendor(e.target.value)}
              />
            </label>
            {defaultVendor !== "" &&
              !vendors.some((v) => v.name === defaultVendor) && (
                <p className="mt-2 flex items-start gap-2 text-xs leading-relaxed text-amber-ink">
                  <span className="lamp mt-1" aria-hidden />
                  No connected vendor is named “{defaultVendor}” right now.
                  Sessions defaulting to it will fail to start until its agent
                  connects.
                </p>
              )}
          </section>
        </div>
      </div>
    </div>
  );
}
