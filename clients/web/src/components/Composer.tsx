import { ArrowUp, Square } from "lucide-react";
import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
} from "react";
import { useTranslation } from "react-i18next";
import { SessionStatusKind, type CatalogEntryView } from "../api/types";
import { UNLOADED, statusMeta } from "../lib/status";
import { EntryMenu, filterEntries, invocationPrefix } from "./EntryMenu";

/**
 * The composer: one field, one button, riding inside it.
 *
 * The button is icon-only. The rule it breaks — "an unlabelled icon is a
 * control you have to learn" — is retired deliberately here and nowhere else:
 * these are the two most-pressed controls in the product, they never move, and
 * an upward arrow beside a text field is not a symbol anyone has to be taught.
 * Both carry their word in `title` and `aria-label`.
 *
 * While a turn runs the button is Stop and only Stop. Queueing the next
 * message is still supported and still useful — Enter does it, and the
 * placeholder says so — but a turn in flight has exactly one thing worth
 * pressing.
 */
export function Composer({
  status,
  busy,
  blockedReason = null,
  idlePlaceholder,
  entries = [],
  onSend,
  onStop,
}: {
  /** `undefined` only while the session document is still being fetched — the
   * server always knows a status. Sending stays enabled meanwhile: sending is
   * queued behind whatever the session turns out to be doing. */
  status: SessionStatusKind | undefined;
  busy: boolean;
  blockedReason?: string | null;
  /** What an idle, unblocked field invites. A workflow run is handed an input
   * rather than sent a message, so the two surfaces do not ask for the same
   * thing. Defaults to the plain "message the agent" invitation. */
  idlePlaceholder?: string;
  /** What the selected bundles offer, for the `/` and `@` typeahead. Empty
   * where there is nothing to complete against. */
  entries?: CatalogEntryView[];
  /** May be async; a rejection means the message never left, and the
   * composer puts it back. */
  onSend: (text: string) => void | Promise<unknown>;
  onStop: () => void;
}) {
  const { t } = useTranslation();
  const [text, setText] = useState("");
  const [active, setActive] = useState(0);
  const ref = useRef<HTMLTextAreaElement>(null);
  const meta = status ? statusMeta(status) : UNLOADED;
  const running = status === SessionStatusKind.Running;
  const awaiting = status === SessionStatusKind.AwaitingInput;
  const blocked = blockedReason != null;

  // Auto-grow the textarea up to a cap. The fractional line box
  // (line-height 1.625 × 15px) leaves sub-pixel overflow below the cap, which
  // browsers that paint scrollbars (Safari zoomed, classic-scrollbar setups)
  // render as a permanent scrollbar on an empty input — so hide overflow
  // until the cap, where the content genuinely overflows and `auto` lets the
  // user scroll.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    const capped = el.scrollHeight > 200;
    el.style.overflowY = capped ? "auto" : "hidden";
    el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
  }, [text]);

  // The menu is open exactly when the field holds a name still being typed.
  // Derived rather than stored, so there is no state to get out of step with
  // the text — deleting back to `/` reopens it, typing a space closes it.
  const invocation = invocationPrefix(text);
  const matches = useMemo(
    () =>
      invocation
        ? filterEntries(entries, invocation.sigil, invocation.query)
        : [],
    [entries, invocation],
  );
  const menuOpen = matches.length > 0;
  const index = Math.min(active, matches.length - 1);

  const pick = (entry: CatalogEntryView) => {
    // Trailing space: the name is chosen, and what follows is arguments.
    setText(`${entry.kind === "agent" ? "@" : "/"}${entry.name} `);
    setActive(0);
    ref.current?.focus();
  };

  const submit = () => {
    const trimmed = text.trim();
    if (!trimmed || !meta.canSend || busy || blocked) return;
    setText("");
    setActive(0);
    // Put it back if it never left. Sending while offline cleared the box and
    // dropped the message: the request failed, the optimistic bubble vanished
    // on the next refetch, and what had been typed existed nowhere.
    void Promise.resolve(onSend(trimmed)).catch(() =>
      setText((current) => (current === "" ? trimmed : current)),
    );
  };

  const onKeyDown = (e: KeyboardEvent) => {
    // The menu owns the keys it needs while it is open. Enter in particular:
    // with the menu up it picks, and sending a half-typed `/rev` instead is
    // the mistake this ordering exists to prevent.
    if (menuOpen) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setActive((i) => (i + 1) % matches.length);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        setActive((i) => (i - 1 + matches.length) % matches.length);
        return;
      }
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        pick(matches[index]);
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        // Nothing to close: the menu is derived from the text, so the way out
        // is to stop the text being an invocation.
        setText(`${text} `);
        return;
      }
    }
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  };

  return (
    <div className="mx-auto w-full max-w-[54rem] px-4 pb-4 sm:px-6">
      {/* The focus ring rides the whole panel, and the textarea inside it is
          `outline-none` on purpose: the field fills the panel, so its own
          2px offset outline had nowhere to go but the sliver below it, and
          the only segment `overflow-hidden` did not clip read as a solid
          coloured line ruled under the input. One control, one ring. */}
      <div className="relative">
        {menuOpen && (
          <EntryMenu entries={matches} activeIndex={index} onPick={pick} />
        )}
        <div className="screen relative overflow-hidden rounded-[var(--radius-panel)] transition-shadow focus-within:shadow-[0_0_0_2px_var(--focus-ring)]">
          <textarea
            ref={ref}
            rows={1}
            value={text}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={onKeyDown}
            data-testid="composer-input"
            aria-label={t("composer.ariaLabel")}
            placeholder={
              !meta.canSend
                ? meta.hint
                : awaiting
                  ? t("composer.answerPlaceholder")
                  : running
                    ? t("composer.queuePlaceholder")
                    : (idlePlaceholder ?? t("composer.idlePlaceholder"))
            }
            disabled={!meta.canSend}
            // `pr-14` reserves the button's lane so a long line never runs
            // underneath it.
            className="max-h-[200px] w-full resize-none bg-transparent py-3 pl-3.5 pr-14 text-[0.9375rem] leading-relaxed text-legend outline-none placeholder:text-faint disabled:opacity-60"
          />

          <div className="absolute bottom-2 right-2">
            {running ? (
              <button
                className="key key-stop !h-8 !w-8 !p-0"
                onClick={onStop}
                disabled={busy}
                title={t("composer.stopTitle")}
                aria-label={t("composer.stop")}
                data-testid="composer-stop"
              >
                <Square size={12} className="fill-current" aria-hidden />
              </button>
            ) : (
              <button
                className="key key-go !h-8 !w-8 !p-0"
                onClick={submit}
                disabled={!text.trim() || !meta.canSend || busy || blocked}
                title={
                  blockedReason ??
                  t("composer.sendTitle")
                }
                aria-label={t("composer.send")}
                data-testid="composer-send"
              >
                <ArrowUp size={15} aria-hidden />
              </button>
            )}
          </div>
        </div>
      </div>

      {blocked && (
        <p
          className="mt-2 px-1 text-xs leading-relaxed text-dim"
          data-testid="composer-blocked-hint"
        >
          {blockedReason}
        </p>
      )}
    </div>
  );
}
