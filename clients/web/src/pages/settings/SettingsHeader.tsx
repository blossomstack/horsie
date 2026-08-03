import { RotateCcw, Save } from "lucide-react";
import { RailToggle } from "../../components/rail";

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
    // The bar spans the pane; its contents share the content column's left
    // edge, so the title sits directly above the first panel rather than
    // floating 100px to its left.
    <header className="border-b bg-panel">
      <div className="mx-auto flex max-w-3xl flex-wrap items-center gap-x-4 gap-y-3 px-4 py-3.5 sm:px-6">
        <RailToggle />
        <div className="min-w-0 flex-1">
          <h1 className="text-[15px] font-semibold tracking-tight text-legend">
            {title}
          </h1>
          <p className="mt-0.5 max-w-prose text-xs leading-relaxed text-faint">
            {desc}
          </p>
        </div>
        {onSave && (
          <div className="flex items-center gap-2">
            {/* Save state is a lamp and a word, like every other state on the
              panel — never the button's colour alone. */}
            {saving ? (
              <span className="flex items-center gap-1.5 text-amber-ink">
                <span className="lamp lamp-live" aria-hidden />
                <span className="legend text-current">Saving</span>
              </span>
            ) : dirty ? (
              <span className="flex items-center gap-1.5 text-amber-ink">
                <span className="lamp" aria-hidden />
                <span className="legend text-current">Unsaved</span>
              </span>
            ) : saved ? (
              <span className="flex items-center gap-1.5 text-lamp-ok">
                <span className="lamp" aria-hidden />
                <span className="legend text-current">Saved</span>
              </span>
            ) : null}
            <button
              className="key key-blank"
              onClick={onDiscard}
              disabled={!dirty}
            >
              <RotateCcw size={13} aria-hidden /> Discard
            </button>
            <button
              className="key key-go"
              onClick={onSave}
              disabled={!dirty || saving}
              data-testid="settings-save"
            >
              <Save size={13} aria-hidden />
              Save
            </button>
          </div>
        )}
      </div>
    </header>
  );
}
