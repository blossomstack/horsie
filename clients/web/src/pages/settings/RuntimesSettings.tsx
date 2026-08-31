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
import { ListRow, RowAction, Rows, RowShell, Section, SettingsPage } from "./fields";
import {
  emptyVendorDraft,
  summarise,
  toVendorInput,
  vendorDraftFrom,
  VendorForm,
  type VendorDraft,
} from "./VendorForm";
import { useTranslation } from "react-i18next";

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
  const { t } = useTranslation();
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
        t("runtimesPage.vendorExists", { name: draft.name.trim() }),
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
      t("runtimesPage.confirmDeleteVendor", { name }),
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
        <span className="lamp lamp-live text-live-ink" aria-hidden />
        <span className="legend">{t("runtimesPage.loading")}</span>
      </div>
    );
  }
  if (error || !settings) {
    return (
      <div className="p-6">
        <p className="notice notice-fault">
{t("runtimesPage.loadFailed")}
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
    <SettingsPage
        title={t("settingsNav.runtimes")}
        saving={update.isPending}
        saved={update.isSuccess && !update.isPending}
    >
        {saveError && (
          <p className="notice notice-fault">
            {saveError}
          </p>
        )}

        <Section
          title={t("runtimesPage.vendors")}
          empty={
            rows.length === 0 && !absentDefault && !adding
              ? t("runtimesPage.empty")
              : null
          }
        >
          {configsFailed && (
            <ReadError
              what={t("runtimesPage.cloudVendors")}
              error={configsError}
              testId="cloud-vendors-error"
            />
          )}

          <Rows>
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
                      <span className="chip">{t("runtimesPage.checking")}</span>
                    ) : (
                      checks[name]?.ok && (
                        <span
                          className="chip text-lamp-ok"
                          data-testid={`cloud-vendor-ok-${name}`}
                        >
                          {t("runtimesPage.answering")}
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
                        <span className="legend text-lamp-ok">{t("runtimesPage.connected")}</span>
                      </>
                    )}
                    {isDefault && <span className="chip">{t("common.default")}</span>}
                  </span>
                }
                actions={
                  <>
                    {!isDefault && (
                      <RowAction
                        icon={<Star size={14} />}
                        label={t("runtimesPage.makeDefault", { name })}
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
                          label={t("runtimesPage.check", { name })}
                          onClick={() => void check(name)}
                          disabled={checking !== null}
                          testId={`cloud-vendor-test-${name}`}
                        />
                        <RowAction
                          icon={<Pencil size={14} />}
                          label={t("runtimesPage.edit", { name })}
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
                          label={t("common.deleteNamed", { name })}
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
                    className="notice notice-fault"
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
              subtitle={t("runtimesPage.absentDefault")}
              meta={
                <span className="flex shrink-0 items-center gap-2">
                  <span className="lamp lamp-off text-faint" aria-hidden />
                  <span className="legend">{t("runtimesPage.notConnected")}</span>
                  <span className="chip">{t("common.default")}</span>
                </span>
              }
              actions={
                <RowAction
                  icon={<X size={14} />}
                  label={t("runtimesPage.clearDefault")}
                  onClick={() => void makeDefault(null)}
                  disabled={update.isPending}
                  testId="vendor-clear-default"
                />
              }
            />
          )}
          </Rows>

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
                {t("runtimesPage.addFly")}
              </button>
              <button
                className="key"
                onClick={() => openEditor(emptyVendorDraft("Velos"))}
                data-testid="cloud-vendor-add-velos"
              >
                {t("runtimesPage.addVelos")}
              </button>
            </div>
          )}
        </Section>
      </SettingsPage>
  );
}
