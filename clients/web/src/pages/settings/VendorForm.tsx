import type {
  FlyVendorSettings,
  RuntimeVendorConfigInput,
  RuntimeVendorConfigView,
  RuntimeVendorSettings,
  VelosVendorSettings,
} from "../../api/types";
import { TextField } from "./fields";
import { useTranslation } from "react-i18next";

/**
 * The fields of one cloud runtime vendor, and nothing about which vendors
 * exist.
 *
 * A vendor process that dials in carries its own configuration, so there is
 * nothing to edit for one; a cloud vendor has nowhere to dial in from, so this
 * form is the only thing that makes it exist. `RuntimesSettings` owns the list
 * and the mutations and renders this inside whichever row is being edited.
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

export type VendorDraft = {
  name: string;
  settings: RuntimeVendorSettings;
  /** Empty means "keep the stored token" — one is never readable back. */
  credential: string;
  /** Editing an existing vendor, so its kind is fixed and a blank token is OK. */
  existing: boolean;
};

export function emptyVendorDraft(
  kind: RuntimeVendorSettings["kind"],
): VendorDraft {
  return {
    name: "",
    settings:
      kind === "Fly"
        ? { kind: "Fly", value: { ...EMPTY_FLY } }
        : { kind: "Velos", value: { ...EMPTY_VELOS } },
    credential: "",
    existing: false,
  };
}

export function vendorDraftFrom(v: RuntimeVendorConfigView): VendorDraft {
  return { name: v.name, settings: v.settings, credential: "", existing: true };
}

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

/** The draft as the save body, with the callback URL completed. */
export function toVendorInput(draft: VendorDraft): RuntimeVendorConfigInput {
  return {
    name: draft.name,
    settings: {
      kind: draft.settings.kind,
      value: {
        ...draft.settings.value,
        callbackUrl: withConnectPath(callbackOf(draft.settings)),
      },
    } as RuntimeVendorSettings,
    credential: draft.credential || undefined,
  };
}

/** One line saying what a vendor is made of, for its row. */
export function summarise(v: RuntimeVendorConfigView): string {
  return v.settings.kind === "Fly"
    ? `Fly · ${v.settings.value.region} · ${v.settings.value.image || "no image"}`
    : `velos · ${v.settings.value.serverUrl || "no server"} · ${v.settings.value.image || "no image"}`;
}

export function VendorForm({
  draft,
  setDraft,
  onSave,
  onCancel,
  saving,
  error,
}: {
  draft: VendorDraft;
  setDraft: (next: VendorDraft) => void;
  onSave: () => void;
  onCancel: () => void;
  saving: boolean;
  error: string | null;
}) {
  const { t } = useTranslation();
  /** Patch the settings of whichever kind the draft is, keeping the tag. */
  const patch = (
    values: Partial<FlyVendorSettings> & Partial<VelosVendorSettings>,
  ) =>
    setDraft({
      ...draft,
      settings: {
        kind: draft.settings.kind,
        value: { ...draft.settings.value, ...values },
      } as RuntimeVendorSettings,
    });

  // A number field that has been cleared is not a zero — treating it as one
  // would trip the "needs at least one cpu" check while the user is still
  // typing.
  const setNumber = (key: string, raw: string) =>
    patch({ [key]: raw === "" ? 0 : Number(raw) });

  return (
    <>
      <div className="grid gap-3 sm:grid-cols-2">
        <TextField
          label={t("vendorForm.name")}
          value={draft.name}
          onChange={(name) => setDraft({ ...draft, name })}
          placeholder={draft.settings.kind === "Fly" ? "fly" : "velos"}
        />
        {draft.settings.kind === "Fly" ? (
          <TextField
            label={t("vendorForm.flyApp")}
            value={draft.settings.value.app}
            onChange={(app) => patch({ app })}
            placeholder={t("vendorForm.flyAppPlaceholder")}
            // horsie never creates the app, and nothing said so: a typo here
            // saved cleanly and failed hours later at the first session, as a
            // machine-create rejection.
            hint={t("vendorForm.flyAppHint")}
          />
        ) : (
          <TextField
            label={t("vendorForm.velosUrl")}
            value={draft.settings.value.serverUrl}
            onChange={(serverUrl) => patch({ serverUrl })}
            placeholder={t("vendorForm.velosUrlPlaceholder")}
          />
        )}
        <TextField
          label={t("vendorForm.apiToken")}
          type="password"
          value={draft.credential}
          onChange={(credential) => setDraft({ ...draft, credential })}
          placeholder={
            draft.existing
              ? t("vendorForm.leaveBlank")
              : draft.settings.kind === "Fly"
                ? t("vendorForm.flyTokenPlaceholder")
                : t("vendorForm.velosTokenPlaceholder")
          }
        />
        {draft.settings.kind === "Fly" && (
          <TextField
            label={t("vendorForm.region")}
            value={draft.settings.value.region}
            onChange={(region) => patch({ region })}
            placeholder={t("vendorForm.regionPlaceholder")}
          />
        )}
        <TextField
          label={t("vendorForm.image")}
          value={draft.settings.value.image}
          onChange={(image) => patch({ image })}
          placeholder={t("vendorForm.imagePlaceholder")}
        />
        <TextField
          label={t("vendorForm.callbackUrl")}
          value={callbackOf(draft.settings)}
          onChange={(callbackUrl) => patch({ callbackUrl })}
          placeholder={
            draft.settings.kind === "Fly"
              ? "wss://horsie.example.com/api/runtime/connect"
              : "ws://horsie.internal:3789/api/runtime/connect"
          }
        />
        <TextField
          label={t("vendorForm.workspaceRoot")}
          value={draft.settings.value.workspaceRoot}
          onChange={(workspaceRoot) => patch({ workspaceRoot })}
          placeholder={t("vendorForm.workspaceRootPlaceholder")}
        />
        <TextField
          label={t("vendorForm.memoryMb")}
          value={String(draft.settings.value.memoryMb)}
          onChange={(v) => setNumber("memoryMb", v)}
        />
        <TextField
          label={t("vendorForm.cpus")}
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
            label={t("vendorForm.volumeSizeGb")}
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
{t("vendorForm.volumesHint")}
        </label>
      ) : null}
      {/* Beside the button that caused it. This sat at the top of the pane,
        which put it 389px above the viewport while SAVE was 542px below — so a
        rejected save looked like a button that did nothing. */}
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
          onClick={onSave}
          disabled={saving}
          data-testid="cloud-vendor-save"
        >
          {saving ? t("common.saving") : t("common.save")}
        </button>
        <button className="key" onClick={onCancel}>
          {t("common.cancel")}
        </button>
      </div>
    </>
  );
}
