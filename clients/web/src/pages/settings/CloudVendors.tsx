import { Pencil } from "lucide-react";
import { useState } from "react";
import { ApiRequestError } from "../../api/client";
import type {
  FlyVendorSettings,
  RuntimeVendorConfigView,
  RuntimeVendorSettings,
  VelosVendorSettings,
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
 * The opposite pole from the connected-vendor roster above it: a process that
 * dials in carries its own configuration, so there is nothing to edit here; a
 * cloud vendor has nowhere to dial in from, so this form is the only thing that
 * makes it exist.
 *
 * The kind is fixed once a vendor is saved. Changing it in place would leave
 * every session pointing at a name whose substrate silently moved — and the
 * runtimes on the old one unreachable and still billing. Deleting and re-adding
 * makes that visible.
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

const EMPTY_VELOS: VelosVendorSettings = {
  serverUrl: "",
  image: "",
  runtimeBin: "horsie-runtime",
  workspaceRoot: "/workspaces",
  callbackUrl: "",
  cpu: 1,
  memoryMb: 1024,
};

type Draft = {
  name: string;
  settings: RuntimeVendorSettings;
  /** Empty means "keep the stored token" — one is never readable back. */
  credential: string;
  /** Editing an existing vendor, so its kind is fixed and a blank token is OK. */
  existing: boolean;
};

/** The callback URL lives under a different key per kind, but means the same. */
function callbackOf(settings: RuntimeVendorSettings): string {
  return settings.value.callbackUrl;
}

const CONNECT_PATH = "/api/runtime/connect";

/**
 * Complete a bare origin with the connect path a runtime dials.
 *
 * The server validates this field and refuses a URL with no path. It used to
 * complete one instead, which is a helpful thing for a form to do and a harmful
 * thing for an API: anything that declares configuration reads back a value it
 * never wrote and cannot tell that from drift. So the affordance lives here,
 * and a URL this cannot parse is passed through untouched — the server refuses
 * it with a message written for whoever typed it.
 */
export function withConnectPath(url: string): string {
  const trimmed = url.trim();
  const authority = trimmed.startsWith("wss://")
    ? trimmed.slice("wss://".length)
    : trimmed.startsWith("ws://")
      ? trimmed.slice("ws://".length)
      : null;
  if (authority === null) return trimmed;
  const [, ...rest] = authority.split("/");
  // A trailing slash is an empty path, not a path: `wss://host/` needs the
  // same completion `wss://host` does.
  if (rest.join("/") !== "") return trimmed;
  return `${trimmed.replace(/\/+$/, "")}${CONNECT_PATH}`;
}

function summarise(v: RuntimeVendorConfigView): string {
  return v.settings.kind === "Fly"
    ? `Fly · ${v.settings.value.region} · ${v.settings.value.image || "no image"}`
    : `velos · ${v.settings.value.serverUrl || "no server"} · ${v.settings.value.image || "no image"}`;
}

export function CloudVendors() {
  const { data: vendors, isLoading } = useRuntimeVendors();
  const save = useSaveRuntimeVendor();
  const remove = useDeleteRuntimeVendor();
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);

  /** Patch the settings of whichever kind the draft is, keeping the tag. */
  const patch = (
    values: Partial<FlyVendorSettings> & Partial<VelosVendorSettings>,
  ) =>
    setDraft((d) =>
      d
        ? {
            ...d,
            settings: {
              kind: d.settings.kind,
              value: { ...d.settings.value, ...values },
            } as RuntimeVendorSettings,
          }
        : d,
    );

  // A number field that has been cleared is not a zero — treating it as one
  // would trip the "needs at least one cpu" check while the user is still
  // typing.
  const setNumber = (key: string, raw: string) =>
    patch({ [key]: raw === "" ? 0 : Number(raw) });

  const submit = async () => {
    if (!draft) return;
    setError(null);
    try {
      await save.mutateAsync({
        name: draft.name,
        body: {
          name: draft.name,
          settings: {
            kind: draft.settings.kind,
            value: {
              ...draft.settings.value,
              callbackUrl: withConnectPath(callbackOf(draft.settings)),
            },
          } as RuntimeVendorSettings,
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

  // A delete is triggered from a row in the list, so there is no one field it
  // belongs under: it reports through the global failure notice instead. The
  // save below keeps its inline error, because that one *does* have a home —
  // directly above the button that caused it.
  const drop = (name: string) => {
    remove.mutate(name, {
      onSuccess: () => {
        if (draft?.name === name) setDraft(null);
      },
    });
  };

  const add = (kind: RuntimeVendorSettings["kind"]) =>
    setDraft({
      name: "",
      settings:
        kind === "Fly"
          ? { kind: "Fly", value: { ...EMPTY_FLY } }
          : { kind: "Velos", value: { ...EMPTY_VELOS } },
      credential: "",
      existing: false,
    });

  return (
    <Section
      title="Cloud vendors"
      desc="Vendors this server runs itself — no process of your own to deploy. Each sandbox dials back to the callback URL, so that URL must be reachable from outside this server."
      empty={
        !isLoading && !draft && (vendors?.length ?? 0) === 0
          ? "No cloud vendors are configured."
          : null
      }
    >
      {vendors?.map((v) => (
        <ListRow
          key={v.name}
          testId={`cloud-vendor-row-${v.name}`}
          title={v.name}
          subtitle={summarise(v)}
          meta={
            <span className="flex shrink-0 items-center gap-2">
              <span className="chip">
                {v.hasCredential ? "Token set" : "No token"}
              </span>
            </span>
          }
          actions={
            <RowAction
              icon={<Pencil size={14} />}
              label={`Edit ${v.name}`}
              onClick={() =>
                setDraft({
                  name: v.name,
                  settings: v.settings,
                  credential: "",
                  existing: true,
                })
              }
              testId={`cloud-vendor-edit-${v.name}`}
            />
          }
        />
      ))}

      {draft ? (
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
              placeholder={draft.settings.kind === "Fly" ? "fly" : "velos"}
            />
            {draft.settings.kind === "Fly" ? (
              <TextField
                label="Fly app"
                value={draft.settings.value.app}
                onChange={(app) => patch({ app })}
                placeholder="horsie-runtimes"
              />
            ) : (
              <TextField
                label="velos server URL"
                value={draft.settings.value.serverUrl}
                onChange={(serverUrl) => patch({ serverUrl })}
                placeholder="http://velos.example:8080"
              />
            )}
            <TextField
              label="API token"
              type="password"
              value={draft.credential}
              onChange={(credential) =>
                setDraft((d) => (d ? { ...d, credential } : d))
              }
              placeholder={
                draft.existing
                  ? "Leave blank to keep"
                  : draft.settings.kind === "Fly"
                    ? "fly api token"
                    : "optional — velos may run without auth"
              }
            />
            {draft.settings.kind === "Fly" && (
              <TextField
                label="Region"
                value={draft.settings.value.region}
                onChange={(region) => patch({ region })}
                placeholder="iad"
              />
            )}
            <TextField
              label="Runtime image"
              value={draft.settings.value.image}
              onChange={(image) => patch({ image })}
              placeholder="ghcr.io/you/horsie-runtime:latest"
            />
            <TextField
              label="Callback URL"
              value={callbackOf(draft.settings)}
              onChange={(callbackUrl) => patch({ callbackUrl })}
              placeholder={
                draft.settings.kind === "Fly"
                  ? "wss://horsie.example.com/api/runtime/connect"
                  : "ws://horsie.internal:3789/api/runtime/connect"
              }
            />
            <TextField
              label="Workspace root"
              value={draft.settings.value.workspaceRoot}
              onChange={(workspaceRoot) => patch({ workspaceRoot })}
              placeholder="/workspaces"
            />
            <TextField
              label="Memory (MB)"
              value={String(draft.settings.value.memoryMb)}
              onChange={(v) => setNumber("memoryMb", v)}
            />
            <TextField
              label="CPUs"
              value={String(
                draft.settings.kind === "Fly"
                  ? draft.settings.value.cpus
                  : draft.settings.value.cpu,
              )}
              onChange={(v) =>
                setNumber(draft.settings.kind === "Fly" ? "cpus" : "cpu", v)
              }
            />
            {draft.settings.kind === "Fly" && (
              <TextField
                label="Volume size (GB)"
                value={String(draft.settings.value.volumeSizeGb)}
                onChange={(v) => setNumber("volumeSizeGb", v)}
              />
            )}
          </div>
          {draft.settings.kind === "Fly" ? (
            <label className="mt-3 flex items-center gap-2 text-xs text-faint">
              <input
                type="checkbox"
                checked={draft.settings.value.volumes}
                onChange={(e) => patch({ volumes: e.target.checked })}
              />
              Give each runtime a volume, so a stopped one keeps its workspace
            </label>
          ) : (
            <p className="mt-3 text-xs leading-relaxed text-faint">
              velos has no volumes: stopping a session deletes its container, and
              the next message schedules a fresh one that re-runs provisioning.
            </p>
          )}
          {/* Beside the button that caused it. This sat at the top of the
            pane, which put it 389px above the viewport while SAVE was 542px
            below — so a rejected save looked like a button that did nothing. */}
          {error && (
            <p
              className="mt-3 rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink"
              data-testid="cloud-vendor-error"
            >
              {error}
            </p>
          )}
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
      ) : (
        <div className="flex gap-2">
          <button
            className="key"
            onClick={() => add("Fly")}
            data-testid="cloud-vendor-add-fly"
          >
            Add Fly
          </button>
          <button
            className="key"
            onClick={() => add("Velos")}
            data-testid="cloud-vendor-add-velos"
          >
            Add velos
          </button>
        </div>
      )}
    </Section>
  );
}
