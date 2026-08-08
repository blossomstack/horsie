import { Star, X } from "lucide-react";
import { useState } from "react";
import { ApiRequestError } from "../../api/client";
import { useSettings, useSetDefaultVendor } from "../../hooks/useSettings";
import { ListRow, RowAction, Section, SettingsPane } from "./fields";
import { SettingsHeader } from "./SettingsHeader";

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
  const update = useSetDefaultVendor();
  const [saveError, setSaveError] = useState<string | null>(null);

  // Setting the default is one action on one row, so there is nothing to batch
  // and no dirty state to publish.
  const makeDefault = async (name: string | null) => {
    setSaveError(null);
    try {
      await update.mutateAsync(name);
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
  // A default naming an agent that has not dialled in is legitimate — the
  // preference applies whenever it connects — so it gets a row of its own
  // rather than disappearing from a list of only-connected vendors.
  const absentDefault =
    settings.defaultVendor &&
    !vendors.some((v) => v.name === settings.defaultVendor)
      ? settings.defaultVendor
      : null;

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <SettingsHeader
        title="Runtimes"
        desc="Where sessions execute. Vendors are agent processes that connect to this server; each one is configured where it runs."
        saving={update.isPending}
        saved={update.isSuccess && !update.isPending}
      />

      <SettingsPane>
        {saveError && (
          <p className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
            {saveError}
          </p>
        )}

        <Section
          title="Connected vendors"
          desc="Agents connected right now. Run horsie connect on a machine, or start a vendor agent such as horsie-velos-runtime, and it appears here. New sessions use the default when they don’t pick one."
          empty={
            vendors.length === 0 && !absentDefault
              ? "No vendor agents are connected, so sessions cannot run a turn yet."
              : null
          }
        >
          {vendors.map((v) => (
            <ListRow
              key={v.name}
              testId={`vendor-row-${v.name}`}
              title={v.name}
              subtitle={
                v.capabilities.supportsProvisioning
                  ? "Provisions repos and skill bundles"
                  : "Works in the agent’s own directories"
              }
              meta={
                <span className="flex shrink-0 items-center gap-2">
                  <span className="lamp text-lamp-ok" aria-hidden />
                  <span className="legend text-lamp-ok">Connected</span>
                  {v.isDefault && <span className="chip">Default</span>}
                </span>
              }
              actions={
                v.isDefault ? undefined : (
                  <RowAction
                    icon={<Star size={14} />}
                    label={`Make ${v.name} the default`}
                    onClick={() => makeDefault(v.name)}
                    disabled={update.isPending}
                    testId={`vendor-make-default-${v.name}`}
                  />
                )
              }
            />
          ))}

          {absentDefault && (
            <ListRow
              testId="vendor-row-absent-default"
              title={absentDefault}
              subtitle="Set as the default, but its agent has not connected. Sessions defaulting to it fail to start until it dials in."
              meta={
                <span className="flex shrink-0 items-center gap-2">
                  <span className="lamp lamp-off text-faint" aria-hidden />
                  <span className="legend">Not connected</span>
                  <span className="chip">Default</span>
                </span>
              }
              actions={
                <RowAction
                  icon={<X size={14} />}
                  label="Clear the default"
                  onClick={() => makeDefault(null)}
                  disabled={update.isPending}
                  testId="vendor-clear-default"
                />
              }
            />
          )}
        </Section>
      </SettingsPane>
    </div>
  );
}
