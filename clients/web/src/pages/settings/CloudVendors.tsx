import { Pencil } from "lucide-react";
import { useState } from "react";
import { ApiRequestError } from "../../api/client";
import type {
  FlyVendorSettings,
  RuntimeVendorConfigView,
} from "../../api/types";
import {
  useDeleteRuntimeVendor,
  useRuntimeVendors,
  useSaveRuntimeVendor,
} from "../../hooks/useRuntimeVendors";
import { ListRow, RowAction, RowShell, Section, TextField } from "./fields";

/**
 * Runtime vendors the server runs itself.
 *
 * The opposite pole from the connected-vendor roster above it: an agent that
 * dials in carries its own configuration, so there is nothing to edit here; a
 * cloud vendor has nowhere to dial in from, so this form is the only thing that
 * makes it exist.
 */

const EMPTY_FLY: FlyVendorSettings = {
  app: "",
  image: "",
  region: "iad",
  workspaceRoot: "/workspaces",
  callbackUrl: "",
  volumes: true,
  cpuKind: "shared",
  cpus: 1,
  memoryMb: 1024,
  volumeSizeGb: 10,
};

type Draft = {
  name: string;
  fly: FlyVendorSettings;
  /** Empty means "keep the stored token" — one is never readable back. */
  credential: string;
  /** Editing an existing vendor, so its name is fixed and a blank token is OK. */
  existing: boolean;
};

function draftOf(v: RuntimeVendorConfigView): Draft {
  return {
    name: v.name,
    fly: v.settings.value,
    credential: "",
    existing: true,
  };
}

export function CloudVendors() {
  const { data: vendors, isLoading } = useRuntimeVendors();
  const save = useSaveRuntimeVendor();
  const remove = useDeleteRuntimeVendor();
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);

  const setFly = (patch: Partial<FlyVendorSettings>) =>
    setDraft((d) => (d ? { ...d, fly: { ...d.fly, ...patch } } : d));

  // A number field that has been cleared is not a zero — treating it as one
  // would trip the "needs at least one cpu" check while the user is still
  // typing.
  const setNumber = (key: keyof FlyVendorSettings, raw: string) =>
    setFly({ [key]: raw === "" ? 0 : Number(raw) } as Partial<FlyVendorSettings>);

  const submit = async () => {
    if (!draft) return;
    setError(null);
    try {
      await save.mutateAsync({
        name: draft.name,
        body: {
          name: draft.name,
          settings: { kind: "Fly", value: draft.fly },
          credential: draft.credential || undefined,
        },
      });
      setDraft(null);
    } catch (e) {
      setError(
        e instanceof ApiRequestError ? e.message : "Failed to save the vendor.",
      );
    }
  };

  const drop = async (name: string) => {
    setError(null);
    try {
      await remove.mutateAsync(name);
      if (draft?.name === name) setDraft(null);
    } catch (e) {
      setError(
        e instanceof ApiRequestError
          ? e.message
          : "Failed to delete the vendor.",
      );
    }
  };

  return (
    <Section
      title="Cloud vendors"
      desc="Vendors this server runs itself. A Fly vendor starts one machine per runtime, and each machine dials back to the callback URL — so that URL must be reachable from outside this server."
      onAdd={
        draft
          ? undefined
          : () =>
              setDraft({
                name: "",
                fly: { ...EMPTY_FLY },
                credential: "",
                existing: false,
              })
      }
      addLabel="Add"
      addTestId="cloud-vendor-add"
      empty={
        !isLoading && !draft && (vendors?.length ?? 0) === 0
          ? "No cloud vendors are configured."
          : null
      }
    >
      {error && (
        <p
          className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink"
          data-testid="cloud-vendor-error"
        >
          {error}
        </p>
      )}

      {vendors?.map((v) => (
        <ListRow
          key={v.name}
          testId={`cloud-vendor-row-${v.name}`}
          title={v.name}
          subtitle={`Fly · ${v.settings.value.region} · ${v.settings.value.image || "no image"}`}
          meta={
            <span className="flex shrink-0 items-center gap-2">
              <span className="chip">{v.hasCredential ? "Token set" : "No token"}</span>
            </span>
          }
          actions={
            <RowAction
              icon={<Pencil size={14} />}
              label={`Edit ${v.name}`}
              onClick={() => setDraft(draftOf(v))}
              testId={`cloud-vendor-edit-${v.name}`}
            />
          }
        />
      ))}

      {draft && (
        <RowShell
          onRemove={() =>
            draft.existing ? void drop(draft.name) : setDraft(null)
          }
          removeLabel={draft.existing ? `Delete ${draft.name}` : "Discard"}
        >
          <div className="grid gap-3 sm:grid-cols-2">
            <TextField
              label="Name"
              value={draft.name}
              onChange={(name) => setDraft((d) => (d ? { ...d, name } : d))}
              placeholder="fly"
            />
            <TextField
              label="Fly app"
              value={draft.fly.app}
              onChange={(app) => setFly({ app })}
              placeholder="horsie-runtimes"
            />
            <TextField
              label="API token"
              type="password"
              value={draft.credential}
              onChange={(credential) =>
                setDraft((d) => (d ? { ...d, credential } : d))
              }
              placeholder={draft.existing ? "Leave blank to keep" : "fly api token"}
            />
            <TextField
              label="Region"
              value={draft.fly.region}
              onChange={(region) => setFly({ region })}
              placeholder="iad"
            />
            <TextField
              label="Runtime image"
              value={draft.fly.image}
              onChange={(image) => setFly({ image })}
              placeholder="ghcr.io/you/horsie-runtime:latest"
            />
            <TextField
              label="Callback URL"
              value={draft.fly.callbackUrl}
              onChange={(callbackUrl) => setFly({ callbackUrl })}
              placeholder="wss://horsie.example.com"
            />
            <TextField
              label="Workspace root"
              value={draft.fly.workspaceRoot}
              onChange={(workspaceRoot) => setFly({ workspaceRoot })}
              placeholder="/workspaces"
            />
            <TextField
              label="Memory (MB)"
              value={String(draft.fly.memoryMb)}
              onChange={(v) => setNumber("memoryMb", v)}
            />
            <TextField
              label="CPUs"
              value={String(draft.fly.cpus)}
              onChange={(v) => setNumber("cpus", v)}
            />
            <TextField
              label="Volume size (GB)"
              value={String(draft.fly.volumeSizeGb)}
              onChange={(v) => setNumber("volumeSizeGb", v)}
            />
          </div>
          <label className="mt-3 flex items-center gap-2 text-xs text-faint">
            <input
              type="checkbox"
              checked={draft.fly.volumes}
              onChange={(e) => setFly({ volumes: e.target.checked })}
            />
            Give each runtime a volume, so a stopped one keeps its workspace
          </label>
          <div className="mt-3 flex gap-2">
            <button
              className="key"
              onClick={() => void submit()}
              disabled={save.isPending}
              data-testid="cloud-vendor-save"
            >
              {save.isPending ? "Saving…" : "Save"}
            </button>
            <button className="key" onClick={() => setDraft(null)}>
              Cancel
            </button>
          </div>
        </RowShell>
      )}
    </Section>
  );
}
