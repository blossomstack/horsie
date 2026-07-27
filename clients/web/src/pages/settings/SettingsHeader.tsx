import { Check, Loader2, RotateCcw, Save } from "lucide-react";

/**
 * The header bar every settings/admin page renders. Pages that own a batched
 * save pass `onSave`/`onDiscard`; self-saving pages (Skills, Memory,
 * Integrations, Model cards) omit them and get title + description only.
 */
export function SettingsHeader({
  title,
  desc,
  dirty = false,
  saved = false,
  saving = false,
  onSave,
  onDiscard,
}: {
  title: string;
  desc: string;
  dirty?: boolean;
  saved?: boolean;
  saving?: boolean;
  onSave?: () => void;
  onDiscard?: () => void;
}) {
  return (
    <header className="flex items-center gap-3 border-b px-6 py-3.5">
      <div>
        <h1 className="text-[15px] font-semibold text-text">{title}</h1>
        <p className="text-xs text-faint">{desc}</p>
      </div>
      {onSave && (
        <div className="ml-auto flex items-center gap-2">
          {dirty && !saving && (
            <span className="text-xs text-faint">Unsaved changes</span>
          )}
          {saved && !dirty && (
            <span className="flex items-center gap-1 text-xs text-success">
              <Check size={13} /> Saved
            </span>
          )}
          <button className="btn-ghost" onClick={onDiscard} disabled={!dirty}>
            <RotateCcw size={14} /> Discard
          </button>
          <button
            className="btn-primary"
            onClick={onSave}
            disabled={!dirty || saving}
            data-testid="settings-save"
          >
            {saving ? (
              <Loader2 size={15} className="animate-spin" />
            ) : (
              <Save size={15} />
            )}
            Save changes
          </button>
        </div>
      )}
    </header>
  );
}
