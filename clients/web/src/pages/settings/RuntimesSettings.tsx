import { Pencil, Star, Stethoscope, Trash2, X } from "lucide-react";
import { useState } from "react";
import { ApiRequestError } from "../../api/client";
import type { RuntimeVendorTestResult } from "../../api/types";
import { ReadError } from "../../components/ReadError";
import {
  useDeleteRuntimeVendor,
  useRuntimeVendors,
  useSaveRuntimeVendor,
  useTestRuntimeVendor,
} from "../../hooks/useRuntimeVendors";
import { useSettings, useSetDefaultRuntimeVendor } from "../../hooks/useSettings";
import { askConfirm } from "../../lib/confirm";
import { ListRow, RowAction, RowShell, Section, SettingsPane } from "./fields";
import { SettingsHeader } from "./SettingsHeader";
import {
  emptyVendorDraft,
  summarise,
  toVendorInput,
  vendorDraftFrom,
  VendorForm,
  type VendorDraft,
} from "./VendorForm";

/**
 * Where sessions execute — one list.
 *
 * There used to be two, and every cloud vendor appeared in both. That was not a
 * rendering mistake but two lists over overlapping sources: `settings.vendors`
 * is the live roster of everything a session can name — agents that dialled in
 * *and* cloud vendors the server runs — while `/api/runtime/vendors` is the
 * configuration of the cloud ones only. Neither is derivable from the other, so
 * they are joined on the name instead of stacked.
 *
 * What a row can do follows from which side it came from. An agent process
 * carries its own configuration where it runs, so there is nothing here to edit
 * for one. A cloud vendor is made entirely of what this page stores, so it gets
 * the form — expanded inside its own row, because an edit that opened at the
 * foot of the page was nowhere near the row that asked for it.
 */
export function RuntimesSettings() {
  const { data: settings, isLoading, error } = useSettings();
  const {
    data: configs,
    isError: configsFailed,
    error: configsError,
  } = useRuntimeVendors();
  const update = useSetDefaultRuntimeVendor();
  const save = useSaveRuntimeVendor();
  const remove = useDeleteRuntimeVendor();
  const test = useTestRuntimeVendor();

  const [saveError, setSaveError] = useState<string | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [draft, setDraft] = useState<VendorDraft | null>(null);
  // Per row, and only for this visit: nothing records a check, because what it
  // reports about a remote credential can stop being true a second later.
  const [checks, setChecks] = useState<Record<string, RuntimeVendorTestResult>>(
    {},
  );
  const [checking, setChecking] = useState<string | null>(null);

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

  const check = async (name: string) => {
    setChecking(name);
    try {
      const result = await test.mutateAsync(name);
      setChecks((c) => ({ ...c, [name]: result }));
    } catch (e) {
      setChecks((c) => ({
        ...c,
        [name]: {
          ok: false,
          error: e instanceof ApiRequestError ? e.message : "The check failed.",
        },
      }));
    } finally {
      setChecking(null);
    }
  };

  const submit = async () => {
    if (!draft) return;
    setFormError(null);
    // The route is an upsert, which is right for an edit and wrong for an add:
    // adding a vendor under a name already taken silently replaced it,
    // destroying its stored credential and repointing every session that names
    // it, with a green Saved. Upsert stays; *add* refuses.
    if (
      !draft.existing &&
      (configs ?? []).some((v) => v.name === draft.name.trim())
    ) {
      return setFormError(
        `A cloud vendor named “${draft.name.trim()}” already exists. Edit it from the list, or pick another name.`,
      );
    }
    try {
      await save.mutateAsync({ name: draft.name, body: toVendorInput(draft) });
      setDraft(null);
    } catch (e) {
      setFormError(
        e instanceof ApiRequestError ? e.message : "Failed to save the vendor.",
      );
    }
  };

  // On the row rather than inside the edit form, where it used to be: a delete
  // you can only reach by first opening an editor is a strange place to keep
  // the one action that cannot be undone. It reports through the global failure
  // notice — a row has no one field an error belongs under.
  const drop = async (name: string) => {
    const ok = await askConfirm(
      `Delete cloud vendor “${name}”? Sessions that name it can no longer start.`,
    );
    if (!ok) return;
    remove.mutate(name, {
      onSuccess: () => {
        if (draft?.name === name) setDraft(null);
      },
    });
  };

  const openEditor = (draft: VendorDraft) => {
    setFormError(null);
    setDraft(draft);
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
  // The join. A saved cloud vendor joins the roster the moment it exists, so
  // the roster is the list; a config with no roster entry is still given a row
  // rather than vanishing, because "I saved it and it disappeared" is the one
  // outcome a settings page must never produce.
  const rows = [
    ...vendors.map((v) => ({
      name: v.name,
      view: v,
      config: configs?.find((c) => c.name === v.name),
    })),
    ...(configs ?? [])
      .filter((c) => !vendors.some((v) => v.name === c.name))
      .map((c) => ({ name: c.name, view: undefined, config: c })),
  ];
  // A default naming an agent that has not dialled in is legitimate — the
  // preference applies whenever it connects — so it gets a row of its own
  // rather than disappearing from a list of only-connected vendors.
  const absentDefault =
    settings.defaultRuntimeVendor &&
    !rows.some((r) => r.name === settings.defaultRuntimeVendor)
      ? settings.defaultRuntimeVendor
      : null;
  const adding = draft && !draft.existing ? draft : null;

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <SettingsHeader
        title="Runtimes"
        desc="Where sessions execute. Agent processes connect to this server and are configured where they run; cloud vendors are configured here."
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
          title="Vendors"
          desc="Run horsie connect on a machine, or start a vendor process such as horsie-velos-runtime, and it appears here. A cloud vendor needs no process of your own — each sandbox dials back to its callback URL, so that URL must be reachable from outside this server. New sessions use the default when they don’t pick one."
          empty={
            rows.length === 0 && !absentDefault && !adding
              ? "No runtimes yet, so sessions cannot run a turn. Connect an agent, or add a cloud vendor below."
              : null
          }
        >
          {configsFailed && (
            <ReadError
              what="cloud vendors"
              error={configsError}
              testId="cloud-vendors-error"
            />
          )}

          {rows.map(({ name, view, config }) => {
            const editing = draft?.existing && draft.name === name;
            const isDefault = view
              ? view.isDefault
              : settings.defaultRuntimeVendor === name;
            return (
              <ListRow
                key={name}
                testId={`vendor-row-${name}`}
                title={name}
                subtitle={
                  config
                    ? summarise(config)
                    : view?.capabilities.supportsProvisioning
                      ? "Provisions repos and skill bundles"
                      : "Works in the agent’s own directories"
                }
                meta={
                  <span className="flex shrink-0 items-center gap-2">
                    {checking === name ? (
                      <span className="chip">Checking…</span>
                    ) : (
                      checks[name]?.ok && (
                        <span
                          className="chip text-lamp-ok"
                          data-testid={`cloud-vendor-ok-${name}`}
                        >
                          Answering
                        </span>
                      )
                    )}
                    {config ? (
                      // A cloud vendor is configuration, not a link: whether
                      // its substrate answers is what Check reports, and a
                      // green lamp beside a revoked token would be a lie.
                      <span className="chip">
                        {config.hasCredential ? "Token set" : "No token"}
                      </span>
                    ) : (
                      <>
                        <span className="lamp text-lamp-ok" aria-hidden />
                        <span className="legend text-lamp-ok">Connected</span>
                      </>
                    )}
                    {isDefault && <span className="chip">Default</span>}
                  </span>
                }
                actions={
                  <>
                    {!isDefault && (
                      <RowAction
                        icon={<Star size={14} />}
                        label={`Make ${name} the default`}
                        onClick={() => void makeDefault(name)}
                        disabled={update.isPending}
                        testId={`vendor-make-default-${name}`}
                      />
                    )}
                    {config && (
                      <>
                        {/* A save proves a vendor before storing it, so this is
                          for what a save cannot see: a token revoked, or an app
                          deleted, since. */}
                        <RowAction
                          icon={<Stethoscope size={14} />}
                          label={`Check ${name}`}
                          onClick={() => void check(name)}
                          disabled={checking !== null}
                          testId={`cloud-vendor-test-${name}`}
                        />
                        <RowAction
                          icon={<Pencil size={14} />}
                          label={`Edit ${name}`}
                          pressed={!!editing}
                          onClick={() =>
                            editing
                              ? setDraft(null)
                              : openEditor(vendorDraftFrom(config))
                          }
                          testId={`cloud-vendor-edit-${name}`}
                        />
                        <RowAction
                          icon={<Trash2 size={14} />}
                          label={`Delete ${name}`}
                          danger
                          disabled={remove.isPending}
                          onClick={() => void drop(name)}
                          testId={`cloud-vendor-delete-${name}`}
                        />
                      </>
                    )}
                  </>
                }
              >
                {checks[name] && !checks[name].ok && (
                  <p
                    className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink"
                    data-testid={`cloud-vendor-test-error-${name}`}
                  >
                    {checks[name].error ?? "The vendor did not answer."}
                  </p>
                )}
                {editing && draft && (
                  <VendorForm
                    draft={draft}
                    setDraft={setDraft}
                    onSave={() => void submit()}
                    onCancel={() => setDraft(null)}
                    saving={save.isPending}
                    error={formError}
                  />
                )}
              </ListRow>
            );
          })}

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
                  onClick={() => void makeDefault(null)}
                  disabled={update.isPending}
                  testId="vendor-clear-default"
                />
              }
            />
          )}

          {/* Adding stays below the list: a vendor that does not exist yet has
            no row to expand into. */}
          {adding ? (
            <RowShell onRemove={() => setDraft(null)} removeLabel="Discard">
              <VendorForm
                draft={adding}
                setDraft={setDraft}
                onSave={() => void submit()}
                onCancel={() => setDraft(null)}
                saving={save.isPending}
                error={formError}
              />
            </RowShell>
          ) : (
            <div className="flex gap-2">
              <button
                className="key"
                onClick={() => openEditor(emptyVendorDraft("Fly"))}
                data-testid="cloud-vendor-add-fly"
              >
                Add Fly
              </button>
              <button
                className="key"
                onClick={() => openEditor(emptyVendorDraft("Velos"))}
                data-testid="cloud-vendor-add-velos"
              >
                Add velos
              </button>
            </div>
          )}
        </Section>
      </SettingsPane>
    </div>
  );
}
