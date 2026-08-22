import { RotateCcw, Save } from "lucide-react";
import { useEffect, useState } from "react";
import { RailToggle } from "../../components/rail";

/**
 * The header bar every settings/admin page renders.
 *
 * Pages that batch a save pass `onSave`/`onDiscard`. Pages that save per item
 * pass only `saving`/`saved` and get the lamp without the buttons — they are
 * the ones that most need it, since they have no button whose state could
 * report the write instead.
 */
export function SettingsHeader({
  title,
  desc,
  dirty = false,
  saved = false,
  saving = false,
  saveBlocked = false,
  onSave,
  onDiscard,
}: {
  title: string;
  desc: string;
  dirty?: boolean;
  saved?: boolean;
  saving?: boolean;
  /** The form is holding a value it will refuse to send. Save goes dead rather
   * than staying lit above the page's own validation message. */
  saveBlocked?: boolean;
  onSave?: () => void;
  onDiscard?: () => void;
}) {
  // "Saved" is an event, not a state. The mutation's `isSuccess` stays true
  // until the next call, so read straight it would latch the lamp on forever
  // after the first write.
  const [showSaved, setShowSaved] = useState(false);
  useEffect(() => {
    if (!saved) return;
    setShowSaved(true);
    const t = setTimeout(() => setShowSaved(false), 2000);
    return () => clearTimeout(t);
  }, [saved]);

  return (
    // The bar spans the pane; its contents share the content column's left
    // edge, so the title sits directly above the first panel rather than
    // floating 100px to its left.
    <header className="bg-panel">
      <div className="mx-auto flex max-w-3xl flex-wrap items-center gap-x-4 gap-y-3 px-4 py-3.5 sm:px-6">
        <RailToggle />
        <div className="min-w-0 flex-1">
          <h1 className="page-title">
            {title}
          </h1>
          <p className="mt-0.5 max-w-prose text-xs leading-relaxed text-faint">
            {desc}
          </p>
        </div>
        {(onSave || saving || showSaved) && (
          <div className="flex items-center gap-2">
            {/* Save state is a lamp and a word, like every other state on the
              panel — never the button's colour alone. Gated on `onSave` alone
              it vanished entirely on the pages that save per item, which are
              exactly the ones with no button to colour instead. */}
            {saving ? (
              <span className="flex items-center gap-1.5 text-live-ink">
                <span className="lamp lamp-live" aria-hidden />
                <span className="legend text-current">Saving</span>
              </span>
            ) : dirty ? (
              <span className="flex items-center gap-1.5 text-live-ink">
                <span className="lamp" aria-hidden />
                <span className="legend text-current">Unsaved</span>
              </span>
            ) : showSaved ? (
              <span className="flex items-center gap-1.5 text-lamp-ok">
                <span className="lamp" aria-hidden />
                <span className="legend text-current">Saved</span>
              </span>
            ) : null}
            {onSave && (
              <>
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
                  disabled={!dirty || saving || saveBlocked}
                  data-testid="settings-save"
                >
                  <Save size={13} aria-hidden />
                  Save
                </button>
              </>
            )}
          </div>
        )}
      </div>
    </header>
  );
}
