import { ChevronRight, History, Loader2, PenLine, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import { askConfirm } from "../../../lib/confirm";
import type { AuthoredPluginView, AuthoredSkillSummary } from "../../../api/types";
import { TextField } from "../fields";
import {
  useCreateAuthoredPlugin,
  useRemoveAuthoredPlugin,
  useRemoveSkill,
  useRestoreSkill,
  useSkillRevisions,
} from "../../../hooks/useAuthored";
import { useTranslation } from "react-i18next";

/**
 * A skill's revisions, fetched only once someone asks for them.
 *
 * The history is the reason the rows are append-only, so it is worth surfacing
 * — but a page listing every plugin must not fetch every skill's history to
 * render a list nobody has opened.
 */
function Revisions({ plugin, skill }: { plugin: string; skill: string }) {
  const { data, isLoading, isError } = useSkillRevisions(plugin, skill, true);
  const restore = useRestoreSkill();
  const { t } = useTranslation();

  if (isLoading) {
    return <p className="px-2 py-1.5 text-xs text-faint">{t("authored.loadingHistory")}</p>;
  }
  if (isError || !data) {
    return (
      <p className="px-2 py-1.5 text-xs text-faint">
        {t("authored.historyFailed")}
      </p>
    );
  }
  return (
    <ul className="mt-1 space-y-1" data-testid="skill-revisions">
      {data.map((r) => (
        <li
          key={r.revision}
          className="flex items-center gap-2 px-2 text-[0.6875rem] text-dim"
        >
          <span className="chip !py-0 text-[0.625rem]">r{r.revision}</span>
          <span className="min-w-0 flex-1 truncate">
            {r.deleted ? <em className="text-faint">{t("authored.deleted")}</em> : r.description}
          </span>
          {/* Restoring the current revision is a no-op that still costs a
              generation bump, so it is not offered. */}
          {!r.deleted && r.revision !== data[0]?.revision && (
            <button
              type="button"
              className="text-faint hover:text-dim"
              onClick={() =>
                restore.mutate({ plugin, skill, revision: r.revision })
              }
            >
              {t("authored.restore")}
            </button>
          )}
        </li>
      ))}
    </ul>
  );
}

function SkillRow({ skill }: { skill: AuthoredSkillSummary }) {
  const { t } = useTranslation();
  const [showHistory, setShowHistory] = useState(false);
  const remove = useRemoveSkill();

  return (
    <li
      className="rounded-[var(--radius-control)] px-2.5 py-2"
      data-testid="authored-skill"
    >
      <div className="flex items-center gap-2">
        <PenLine size={12} className="shrink-0 text-faint" />
        <span className="min-w-0 flex-1">
          <span className="truncate text-sm">{skill.name}</span>
          <span className="ml-2 text-[0.6875rem] text-faint">
            r{skill.revision}
          </span>
          <p className="truncate text-xs text-dim">{skill.description}</p>
        </span>
        <button
          type="button"
          className="text-faint hover:text-dim"
          aria-label={t("authored.historyOf", { name: skill.name })}
          aria-expanded={showHistory}
          onClick={() => setShowHistory((v) => !v)}
        >
          <History size={13} />
        </button>
        <button
          type="button"
          className="text-faint hover:text-[var(--danger)]"
          aria-label={`Delete ${skill.name}`}
          onClick={async () => {
            if (
              await askConfirm(
                t("authored.confirmDeleteSkill", { name: skill.name }),
              )
            ) {
              remove.mutate({ plugin: skill.plugin, skill: skill.name });
            }
          }}
        >
          <Trash2 size={13} />
        </button>
      </div>
      {showHistory && <Revisions plugin={skill.plugin} skill={skill.name} />}
    </li>
  );
}

function PluginRow({ plugin }: { plugin: AuthoredPluginView }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const remove = useRemoveAuthoredPlugin();

  return (
    <div
      className="rounded-[var(--radius-control)] p-3"
      style={{ background: "var(--panel-raised)" }}
      data-testid="authored-plugin-row"
    >
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="item-title truncate">{plugin.name}</span>
            {/* The generation, not a semver: it is what a runtime fetches by,
                and it moves on every edit. */}
            <span className="chip !py-0 text-[0.625rem]">
              gen {plugin.generation}
            </span>
          </div>
          {plugin.description && (
            <p className="mt-0.5 text-xs text-dim">{plugin.description}</p>
          )}
          <button
            type="button"
            className="mt-0.5 flex items-center gap-1 text-[0.6875rem] text-faint hover:text-dim"
            aria-expanded={open}
            onClick={() => setOpen((v) => !v)}
            disabled={plugin.skills.length === 0}
          >
            <ChevronRight
              size={11}
              className={open ? "rotate-90 transition-transform" : "transition-transform"}
            />
            {plugin.skills.length === 1
              ? "1 skill"
              : `${plugin.skills.length} skills`}
          </button>
        </div>
        <button
          type="button"
          className="text-faint hover:text-[var(--danger)]"
          aria-label={t("common.deleteNamed", { name: plugin.name })}
          onClick={async () => {
            if (
              await askConfirm(
                t("authored.confirmDeletePlugin", { name: plugin.name }),
              )
            ) {
              remove.mutate(plugin.name);
            }
          }}
        >
          <Trash2 size={14} />
        </button>
      </div>
      {open && (
        <ul className="mt-2 space-y-1.5">
          {plugin.skills.map((s) => (
            <SkillRow key={s.name} skill={s} />
          ))}
        </ul>
      )}
    </div>
  );
}

/**
 * Plugins written on this server rather than cloned into it.
 *
 * A section of its own rather than rows mixed into the library, because the
 * actions differ: there is nothing to update from, and there is a history to
 * roll back through. The published bundle still appears under Installed
 * bundles, which is where it is switched on for new sessions.
 */
export function AuthoredSection({
  plugins,
}: {
  plugins: AuthoredPluginView[];
}) {
  const [name, setName] = useState("");
  const create = useCreateAuthoredPlugin();
  const { t } = useTranslation();

  return (
    <section className="section" data-testid="authored-section">
      <div className="mb-3 flex items-start gap-2">
        <PenLine size={15} className="mt-0.5 text-faint" />
        <div>
          <h2 className="section-title">{t("authored.title")}</h2>
          <p className="mt-0.5 text-xs text-faint">
{t("authored.desc")}
          </p>
        </div>
      </div>

      {/* The same field and key the install box above uses, so the two ways of
          getting a plugin onto this page read as the same kind of act. */}
      <div className="mb-3">
        <TextField
          label={t("authored.newPlugin")}
          value={name}
          onChange={setName}
          placeholder={t("authored.newPluginPlaceholder")}
          testId="authored-plugin-name"
        />
        {create.isError && (
          <div className="mt-3 rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink">
            {(create.error as Error).message}
          </div>
        )}
        <div className="mt-3 flex justify-end">
          <button
            className="key key-go"
            disabled={!name.trim() || create.isPending}
            onClick={() =>
              create.mutate(
                { name: name.trim(), description: undefined },
                { onSuccess: () => setName("") },
              )
            }
          >
            {create.isPending ? (
              <Loader2 size={15} className="animate-spin" />
            ) : (
              <Plus size={15} />
            )}
            {t("common.create")}
          </button>
        </div>
      </div>

      <div className="space-y-2.5">
        {plugins.length === 0 && (
          <p className="screen px-3 py-4 text-center text-sm text-faint">
{t("authored.empty")}
          </p>
        )}
        {plugins.map((p) => (
          <PluginRow key={p.name} plugin={p} />
        ))}
      </div>
    </section>
  );
}
