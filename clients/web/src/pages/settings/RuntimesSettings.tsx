import { AlertTriangle, ChevronRight, Loader2 } from "lucide-react";
import { useEffect, useState } from "react";
import { ApiRequestError } from "../../api/client";
import type {
  SettingsView,
  VendorInput,
  VendorTestResult,
} from "../../api/types";
import { cn } from "../../lib/cn";
import { useSettings, useTestVendor, useUpdateSettings } from "../../hooks/useSettings";
import { RowShell, Section, TextField } from "./fields";
import { SettingsHeader } from "./SettingsHeader";

type VelosDraft = {
  name: string;
  serverUrl: string;
  image: string;
  advertiseAddress: string;
  tokenInput: string; // "" = keep stored token
  hasInlineToken: boolean;
  runtimeBin: string;
  workspaceRoot: string;
  cpu: string;
  memoryMib: string;
  connectTimeoutSecs: string;
  active: boolean;
  error: string | null;
};

const num = (n: number | undefined): string => (n != null ? String(n) : "");

// Velos drafts come from the generic vendor list: the vendors whose config is
// the `Velos` variant. Their name lives on the vendor, the fields on the config.
const toVelosDrafts = (v: SettingsView): VelosDraft[] =>
  v.vendors.flatMap((vd) =>
    vd.config?.kind === "Velos"
      ? [
          {
            name: vd.name,
            serverUrl: vd.config.value.serverUrl,
            image: vd.config.value.image,
            advertiseAddress: vd.config.value.advertiseAddress,
            tokenInput: "",
            hasInlineToken: vd.config.value.hasInlineToken,
            runtimeBin: vd.config.value.runtimeBin,
            workspaceRoot: vd.config.value.workspaceRoot,
            cpu: num(vd.config.value.cpu),
            memoryMib: num(vd.config.value.memoryMib),
            connectTimeoutSecs: num(vd.config.value.connectTimeoutSecs),
            active: vd.active,
            error: vd.error ?? null,
          },
        ]
      : [],
  );

/**
 * Where sessions execute: the default vendor plus any velos clusters. Saves
 * only `vendors` + `defaultVendor`, leaving the Models page's collections
 * alone.
 */
export function RuntimesSettings() {
  const { data: settings, isLoading, isError } = useSettings();
  const update = useUpdateSettings();
  const testVendor = useTestVendor();

  const [velos, setVelos] = useState<VelosDraft[]>([]);
  const [defaultVendor, setDefaultVendor] = useState("");
  const [dirty, setDirty] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
  const [velosTests, setVelosTests] = useState<
    Record<string, { pending: boolean; result: VendorTestResult | null }>
  >({});

  const runVelosTest = async (name: string) => {
    setVelosTests((m) => ({
      ...m,
      [name]: { pending: true, result: m[name]?.result ?? null },
    }));
    try {
      const result = await testVendor.mutateAsync(name);
      setVelosTests((m) => ({ ...m, [name]: { pending: false, result } }));
    } catch (e) {
      setVelosTests((m) => ({
        ...m,
        [name]: {
          pending: false,
          result: {
            ok: false,
            identity: undefined,
            error: e instanceof ApiRequestError ? e.message : "Test failed.",
          },
        },
      }));
    }
  };

  // (Re)seed the form from the server view on load and after a successful save.
  useEffect(() => {
    if (!settings) return;
    setVelos(toVelosDrafts(settings));
    setDefaultVendor(settings.defaultVendor);
    setDirty(false);
    setLocalError(null);
  }, [settings]);

  const touch = () => setDirty(true);

  const save = () => {
    setLocalError(null);
    const uniq = (xs: string[]) => new Set(xs).size === xs.length;
    if (velos.some((v) => !v.name.trim()))
      return setLocalError("Every velos vendor needs a name.");
    if (!uniq(velos.map((v) => v.name.trim())))
      return setLocalError("Velos vendor names must be unique.");
    for (const v of velos) {
      if (!v.serverUrl.trim() || !v.image.trim() || !v.advertiseAddress.trim())
        return setLocalError(
          `Velos vendor "${v.name}" needs a server URL, image, and advertise address.`,
        );
      if (v.advertiseAddress.trim() && !/^[^:]+:\d+$/.test(v.advertiseAddress.trim()))
        return setLocalError(
          `Advertise address for "${v.name}" must be host:port.`,
        );
      for (const [label, val] of [
        ["CPU", v.cpu],
        ["memory", v.memoryMib],
        ["connect timeout", v.connectTimeoutSecs],
      ] as const)
        if (val.trim() && !/^\d+$/.test(val.trim()))
          return setLocalError(`${label} for "${v.name}" must be a number.`);
    }

    const vendorInputs: VendorInput[] = velos.map((v) => ({
      name: v.name.trim(),
      config: {
        kind: "Velos",
        value: {
          serverUrl: v.serverUrl.trim(),
          image: v.image.trim(),
          advertiseAddress: v.advertiseAddress.trim(),
          token: v.tokenInput === "" ? undefined : v.tokenInput,
          runtimeBin: v.runtimeBin.trim() || undefined,
          workspaceRoot: v.workspaceRoot.trim() || undefined,
          cpu: v.cpu.trim() ? Number(v.cpu.trim()) : undefined,
          memoryMib: v.memoryMib.trim() ? Number(v.memoryMib.trim()) : undefined,
          connectTimeoutSecs: v.connectTimeoutSecs.trim()
            ? Number(v.connectTimeoutSecs.trim())
            : undefined,
        },
      },
    }));

    update.mutate(
      {
        vendors: vendorInputs,
        defaultVendor: defaultVendor || undefined,
      },
      {
        onSuccess: (view) => {
          for (const vd of view.vendors) {
            if (vd.config?.kind === "Velos") runVelosTest(vd.name);
          }
        },
      },
    );
  };

  const discard = () => {
    if (!settings) return;
    setVelos(toVelosDrafts(settings));
    setDefaultVendor(settings.defaultVendor);
    setDirty(false);
    setLocalError(null);
    update.reset();
  };

  const saveError =
    update.error instanceof ApiRequestError
      ? update.error.message
      : update.isError
        ? "Failed to save settings."
        : null;

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <SettingsHeader
        title="Runtimes"
        desc="Where sessions execute — the default vendor and any velos clusters."
        dirty={dirty}
        saved={update.isSuccess}
        saving={update.isPending}
        onSave={save}
        onDiscard={discard}
      />

      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-3xl space-y-6 px-6 py-6">
          {isLoading && (
            <div className="py-16 text-center text-sm text-faint">Loading…</div>
          )}
          {isError && (
            <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
              Couldn’t load settings. Is <code>horsie serve</code> running?
            </div>
          )}

          {(localError || saveError) && (
            <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
              {localError ?? saveError}
            </div>
          )}
          {settings?.restartRequired && (
            <div className="flex items-start gap-2 rounded-[var(--radius)] border border-warning/40 bg-warning-soft px-3 py-2 text-sm text-warning">
              <AlertTriangle size={15} className="mt-0.5 shrink-0" />
              A vendor's server URL, listen address, or advertise host changed
              and needs a server restart to take effect. Other vendor edits
              apply immediately.
            </div>
          )}

          {settings && (
            <>
              <VendorsCard
                view={settings}
                defaultVendor={defaultVendor}
                onChange={(v) => {
                  setDefaultVendor(v);
                  touch();
                }}
              />

              <Section
                title="Velos remote runtimes"
                desc="Remote container runtimes (velos clusters). Add as many as you need — all changes apply immediately."
                onAdd={() => {
                  setVelos((vs) => [
                    ...vs,
                    {
                      name: "",
                      serverUrl: "",
                      image: "",
                      advertiseAddress: "",
                      tokenInput: "",
                      hasInlineToken: false,
                      runtimeBin: "",
                      workspaceRoot: "",
                      cpu: "",
                      memoryMib: "",
                      connectTimeoutSecs: "",
                      active: false,
                      error: null,
                    },
                  ]);
                  touch();
                }}
                addLabel="Add velos vendor"
                empty={velos.length === 0 ? "No velos vendors — sessions run locally." : null}
              >
                {velos.map((v, i) => (
                  <VelosRow
                    key={i}
                    draft={v}
                    onChange={(next) => {
                      setVelos((vs) => vs.map((x, j) => (j === i ? next : x)));
                      touch();
                    }}
                    onRemove={() => {
                      setVelos((vs) => vs.filter((_, j) => j !== i));
                      touch();
                    }}
                    testDisabled={dirty}
                    test={velosTests[v.name]}
                    onTest={() => runVelosTest(v.name)}
                  />
                ))}
              </Section>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function VelosRow({
  draft,
  onChange,
  onRemove,
  testDisabled,
  test,
  onTest,
}: {
  draft: VelosDraft;
  onChange: (next: VelosDraft) => void;
  onRemove: () => void;
  testDisabled: boolean;
  test: { pending: boolean; result: VendorTestResult | null } | undefined;
  onTest: () => void;
}) {
  const [advanced, setAdvanced] = useState(false);
  const set = (patch: Partial<VelosDraft>) => onChange({ ...draft, ...patch });
  return (
    <RowShell onRemove={onRemove} removeLabel="Remove velos vendor">
      <div className="space-y-3">
        <div className="grid grid-cols-2 gap-3">
          <TextField label="Name" value={draft.name} onChange={(v) => set({ name: v })} placeholder="cluster-a" />
          <TextField
            label="Server URL"
            value={draft.serverUrl}
            onChange={(v) => set({ serverUrl: v })}
            placeholder="http://velos.internal:8080"
          />
          <TextField
            label="Runtime image"
            value={draft.image}
            onChange={(v) => set({ image: v })}
            placeholder="ghcr.io/…/horsie-runtime:tag"
          />
          <TextField
            label="Advertise address"
            value={draft.advertiseAddress}
            onChange={(v) => set({ advertiseAddress: v })}
            placeholder="10.0.0.5:3789"
          />
          <TextField
            label="Inline token"
            type="password"
            value={draft.tokenInput}
            onChange={(v) => set({ tokenInput: v })}
            placeholder={draft.hasInlineToken ? "•••• stored — blank keeps it" : "not set"}
          />
        </div>

        <button
          type="button"
          className="flex items-center gap-1 text-xs font-medium text-muted transition-colors hover:text-text"
          onClick={() => setAdvanced((a) => !a)}
        >
          <ChevronRight size={13} className={cn("transition-transform", advanced && "rotate-90")} />
          Advanced
        </button>
        {advanced && (
          <div className="grid grid-cols-2 gap-3 border-t pt-3">
            <TextField
              label="Runtime bin"
              value={draft.runtimeBin}
              onChange={(v) => set({ runtimeBin: v })}
              placeholder="horsie-runtime"
            />
            <TextField
              label="Workspace root"
              value={draft.workspaceRoot}
              onChange={(v) => set({ workspaceRoot: v })}
              placeholder="/workspace"
            />
            <TextField label="CPU" value={draft.cpu} onChange={(v) => set({ cpu: v })} placeholder="2" />
            <TextField
              label="Memory (MiB)"
              value={draft.memoryMib}
              onChange={(v) => set({ memoryMib: v })}
              placeholder="1024"
            />
            <TextField
              label="Connect timeout (s)"
              value={draft.connectTimeoutSecs}
              onChange={(v) => set({ connectTimeoutSecs: v })}
              placeholder="60"
            />
          </div>
        )}
        <div className="flex items-center gap-2">
          <button
            type="button"
            className="btn-outline text-xs"
            disabled={testDisabled || test?.pending}
            title={testDisabled ? "Save changes to test" : undefined}
            onClick={onTest}
          >
            {test?.pending && <Loader2 size={13} className="animate-spin" />}
            Test connection
          </button>
          {test?.result &&
            (test.result.ok ? (
              <span className="chip !py-0 text-[10px] text-success">
                Connected as {test.result.identity}
              </span>
            ) : (
              <span
                className="truncate text-[11px] text-error"
                title={test.result.error ?? undefined}
              >
                {test.result.error}
              </span>
            ))}
        </div>
        {draft.error && <p className="text-[11px] text-error">{draft.error}</p>}
        {!draft.active && !draft.error && draft.name.trim() && (
          <p className="text-[11px] text-faint">Not loaded yet.</p>
        )}
      </div>
    </RowShell>
  );
}

function VendorsCard({
  view,
  defaultVendor,
  onChange,
}: {
  view: SettingsView;
  defaultVendor: string;
  onChange: (v: string) => void;
}) {
  return (
    <section className="card p-4">
      <h2 className="text-sm font-semibold text-text">Default vendor</h2>
      <p className="mt-0.5 text-xs text-faint">
        Where new sessions run unless they pick another. Only loaded vendors can
        be the default.
      </p>
      <div className="mt-3 space-y-1.5">
        {view.vendors.map((v) => (
          <label
            key={v.name}
            className="flex items-center gap-2.5 rounded-[var(--radius)] border px-3 py-2 text-sm"
            style={{ background: "var(--surface-2)" }}
          >
            <input
              type="radio"
              name="default-vendor"
              className="accent-[var(--accent)]"
              checked={defaultVendor === v.name}
              disabled={!v.active}
              onChange={() => onChange(v.name)}
            />
            <span className="font-mono text-text">{v.name}</span>
            {!v.active && <span className="chip !py-0 text-[10px]">not loaded</span>}
            {defaultVendor === v.name && (
              <span className="ml-auto text-xs text-faint">default</span>
            )}
          </label>
        ))}
      </div>
    </section>
  );
}
