import { ArrowUp, FileText, Loader2, Paperclip, Square, X } from "lucide-react";
import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type ClipboardEvent,
  type DragEvent,
  type KeyboardEvent,
} from "react";
import { useTranslation } from "react-i18next";
import { api, ApiRequestError } from "../api/client";
import {
  SessionStatusKind,
  type ArtifactRef,
  type CatalogEntryView,
} from "../api/types";
import { cn } from "../lib/cn";
import { UNLOADED, statusMeta } from "../lib/status";
import { EntryMenu, filterEntries, invocationPrefix } from "./EntryMenu";

/** What the server stores, and therefore all this offers to send. */
const IMAGE_TYPES = ["image/png", "image/jpeg", "image/gif", "image/webp"];
const DOCUMENT_TYPES = ["application/pdf"];

/**
 * One file in the tray.
 *
 * It exists from the moment it is chosen, not from the moment it is stored:
 * the upload runs on attach, so there is a window — the whole upload — during
 * which there is something to show and no `ArtifactRef` to show it with.
 * `previewUrl` is a blob URL for exactly that window, and it outlives the
 * upload only because re-fetching bytes this tab already holds to redraw a
 * 56px square would be silly.
 */
interface Attachment {
  localId: string;
  name: string;
  isImage: boolean;
  /** Absent for a document, and for anything restored from a stored draft. */
  previewUrl?: string;
  status: "uploading" | "ready" | "error";
  ref?: ArtifactRef;
  error?: string;
}

let attachSeq = 0;

/**
 * The composer: one field, one button, riding inside it.
 *
 * The button is icon-only. The rule it breaks — "an unlabelled icon is a
 * control you have to learn" — is retired deliberately here and nowhere else:
 * these are the two most-pressed controls in the product, they never move, and
 * an upward arrow beside a text field is not a symbol anyone has to be taught.
 * Both carry their word in `title` and `aria-label`. The paperclip beside them
 * is the third and last, on the same terms.
 *
 * While a turn runs the button is Stop and only Stop. Queueing the next
 * message is still supported and still useful — Enter does it, and the
 * placeholder says so — but a turn in flight has exactly one thing worth
 * pressing.
 *
 * An attachment uploads the moment it is attached, not when the message is
 * sent. Sending then stays instant however large the picture is, and the wait
 * happens where there is something to show for it — a thumbnail is already on
 * screen with a spinner over it while the bytes go up.
 */
export function Composer({
  status,
  busy,
  blockedReason = null,
  idlePlaceholder,
  entries = [],
  canAttachImages = true,
  canAttachDocuments = true,
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
  /** Whether pictures may be attached here. Default `true`: the model-side
   * capability flags that will decide this are not on the wire yet, and a
   * composer that refused everything until they arrive would be a regression
   * against a feature that has never shipped. */
  canAttachImages?: boolean;
  /** Whether documents (PDFs) may be attached here. Same default, same
   * reason. */
  canAttachDocuments?: boolean;
  /** May be async; a rejection means the message never left, and the
   * composer puts it — and everything attached to it — back. */
  onSend: (text: string, artifacts: ArtifactRef[]) => void | Promise<unknown>;
  onStop: () => void;
}) {
  const { t } = useTranslation();
  const [text, setText] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [notice, setNotice] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);
  const [active, setActive] = useState(0);
  const ref = useRef<HTMLTextAreaElement>(null);
  const fileRef = useRef<HTMLInputElement>(null);
  const meta = status ? statusMeta(status) : UNLOADED;
  const running = status === SessionStatusKind.Running;
  const awaiting = status === SessionStatusKind.AwaitingInput;
  const blocked = blockedReason != null;

  const accepted = useMemo(
    () => [
      ...(canAttachImages ? IMAGE_TYPES : []),
      ...(canAttachDocuments ? DOCUMENT_TYPES : []),
    ],
    [canAttachImages, canAttachDocuments],
  );
  // One reason, shown in three places: the button's tooltip, the notice a
  // paste raises, and — because a disabled control that says nothing is a
  // dead end — nowhere else.
  const attachReason =
    accepted.length === 0 ? t("composer.attachUnavailable") : null;

  // Blob URLs are a resource the browser will not reclaim on its own. Tracked
  // in a ref rather than derived from `attachments`, because the cleanup runs
  // at unmount, by which time the state this component closed over is gone.
  const blobUrls = useRef<string[]>([]);
  useEffect(
    () => () => {
      for (const url of blobUrls.current) URL.revokeObjectURL(url);
      blobUrls.current = [];
    },
    [],
  );

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

  /**
   * Take files, refuse what this session cannot carry, and start an upload
   * per survivor. Returns whether anything was taken, which is what decides
   * if a paste still gets to insert its text.
   */
  const attach = (files: readonly File[]): boolean => {
    if (files.length === 0) return false;
    if (attachReason) {
      setNotice(attachReason);
      return false;
    }
    const allowed = files.filter((f) => accepted.includes(f.type));
    setNotice(
      allowed.length === files.length ? null : t("composer.attachRefused"),
    );
    for (const file of allowed) {
      const localId = `att-${attachSeq++}`;
      const isImage = IMAGE_TYPES.includes(file.type);
      // Only for a picture: a blob URL for a PDF would be a resource held
      // open for a chip that draws an icon.
      const previewUrl = isImage ? URL.createObjectURL(file) : undefined;
      if (previewUrl) blobUrls.current.push(previewUrl);
      setAttachments((list) => [
        ...list,
        {
          localId,
          // A pasted screenshot arrives as `image.png`, and a `File` with no
          // name at all is possible; neither is worth showing as nothing.
          name: file.name || t("composer.pastedName"),
          isImage,
          previewUrl,
          status: "uploading",
        },
      ]);
      const settle = (patch: Partial<Attachment>) =>
        setAttachments((list) =>
          list.map((a) => (a.localId === localId ? { ...a, ...patch } : a)),
        );
      void (async () => {
        try {
          settle({ status: "ready", ref: await api.artifacts.upload(file) });
        } catch (e) {
          settle({
            status: "error",
            error:
              e instanceof ApiRequestError ? e.message : t("composer.uploadFailed"),
          });
        }
      })();
    }
    return allowed.length > 0;
  };

  const remove = (localId: string) => {
    setAttachments((list) => {
      const gone = list.find((a) => a.localId === localId);
      if (gone?.previewUrl) {
        URL.revokeObjectURL(gone.previewUrl);
        blobUrls.current = blobUrls.current.filter(
          (u) => u !== gone.previewUrl,
        );
      }
      return list.filter((a) => a.localId !== localId);
    });
    setNotice(null);
  };

  // Nothing may be sent while a file is still going up or has failed to: the
  // first would drop an attachment that is about to exist, the second would
  // drop one the user can still see.
  const unsettled = attachments.some((a) => a.status !== "ready");
  const ready = attachments.filter((a) => a.ref !== undefined);
  const hasContent = text.trim().length > 0 || ready.length > 0;

  const submit = () => {
    const trimmed = text.trim();
    if (!hasContent || !meta.canSend || busy || blocked || unsettled) return;
    const sent = ready.map((a) => a.ref!);
    setText("");
    setAttachments([]);
    setNotice(null);
    setActive(0);
    // Put it back if it never left. Sending while offline cleared the box and
    // dropped the message: the request failed, the optimistic bubble vanished
    // on the next refetch, and what had been typed existed nowhere. The
    // attachments go back with it — they are as much of the message as the
    // sentence is, and the bytes are still on the server, so the refs are
    // still good.
    void Promise.resolve(onSend(trimmed, sent)).catch(() => {
      setText((current) => (current === "" ? trimmed : current));
      setAttachments((current) => (current.length === 0 ? ready : current));
    });
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

  const onPaste = (e: ClipboardEvent<HTMLTextAreaElement>) => {
    const files = Array.from(e.clipboardData?.files ?? []);
    if (files.length === 0) return;
    // A screenshot on the clipboard is a file *and* nothing else, so taking
    // it means the default paste has nothing left to do. Prevented only when
    // something was actually taken — a rejected file must still let whatever
    // text rode along with it land in the field.
    if (attach(files)) e.preventDefault();
  };

  const onDrop = (e: DragEvent) => {
    e.preventDefault();
    setDragging(false);
    attach(Array.from(e.dataTransfer?.files ?? []));
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
        <div
          className={cn(
            "screen relative overflow-hidden rounded-[var(--radius-panel)] transition-shadow focus-within:shadow-[0_0_0_2px_var(--focus-ring)]",
            // The same ring the field wears when focused, so a drag reads as
            // "this is where it goes" without a second visual language.
            dragging && "shadow-[0_0_0_2px_var(--focus-ring)]",
          )}
          onDragOver={(e) => {
            if (attachReason) return;
            e.preventDefault();
            setDragging(true);
          }}
          onDragLeave={(e) => {
            // Only when the pointer left the panel itself: `dragleave` fires
            // for every child it crosses on the way in.
            if (e.currentTarget.contains(e.relatedTarget as Node | null)) return;
            setDragging(false);
          }}
          onDrop={onDrop}
          data-testid="composer-panel"
          data-dragging={dragging ? "true" : undefined}
        >
          <textarea
            ref={ref}
            rows={1}
            value={text}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={onKeyDown}
            onPaste={onPaste}
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
            // `pr-24` reserves the button lane so a long line never runs
            // underneath it.
            className="max-h-[200px] w-full resize-none bg-transparent py-3 pl-3.5 pr-24 text-[0.9375rem] leading-relaxed text-legend outline-none placeholder:text-faint disabled:opacity-60"
          />

          {attachments.length > 0 && (
            // Padded on the right for the same reason the field is: the
            // button lane floats over this row's end.
            <div
              className="flex flex-wrap items-start gap-2 px-3 pb-2.5 pr-24"
              data-testid="composer-tray"
            >
              {attachments.map((a) => (
                <TrayItem key={a.localId} item={a} onRemove={remove} />
              ))}
            </div>
          )}

          <div className="absolute bottom-2 right-2 flex items-center gap-1">
            <input
              ref={fileRef}
              type="file"
              multiple
              accept={accepted.join(",")}
              className="hidden"
              data-testid="composer-file-input"
              onChange={(e) => {
                attach(Array.from(e.target.files ?? []));
                // Cleared so choosing the same file twice in a row still
                // fires `change`.
                e.target.value = "";
              }}
            />
            <button
              type="button"
              className="key key-blank !h-8 !w-8 !p-0"
              onClick={() => fileRef.current?.click()}
              disabled={attachReason !== null || !meta.canSend || busy}
              title={attachReason ?? t("composer.attachTitle")}
              aria-label={t("composer.attach")}
              data-testid="composer-attach"
            >
              <Paperclip size={14} aria-hidden />
            </button>
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
                disabled={
                  !hasContent || !meta.canSend || busy || blocked || unsettled
                }
                title={
                  blockedReason ??
                  (unsettled
                    ? t("composer.attachPending")
                    : t("composer.sendTitle"))
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
      {notice && (
        <p
          className="mt-2 px-1 text-xs leading-relaxed text-dim"
          data-testid="composer-attach-notice"
        >
          {notice}
        </p>
      )}
    </div>
  );
}

/** One tray entry: a thumbnail for a picture, a chip for a document, and in
 * both cases whatever the upload is currently doing. */
function TrayItem({
  item,
  onRemove,
}: {
  item: Attachment;
  onRemove: (localId: string) => void;
}) {
  const { t } = useTranslation();
  const src =
    item.previewUrl ?? (item.ref ? api.artifactUrl(item.ref.id) : undefined);

  return (
    <div
      className="relative"
      data-testid="composer-attachment"
      data-status={item.status}
      data-name={item.name}
      title={item.error ? `${item.name} — ${item.error}` : item.name}
    >
      {item.isImage && src ? (
        <img
          src={src}
          alt={item.name}
          className={cn(
            "h-14 w-14 rounded-[var(--radius-control)] bg-raised object-cover",
            item.status !== "ready" && "opacity-40",
          )}
        />
      ) : (
        <div className="flex h-14 max-w-44 items-center gap-2 rounded-[var(--radius-control)] bg-raised px-2.5">
          <FileText
            size={14}
            className={cn(
              "shrink-0",
              item.status === "error" ? "text-red-ink" : "text-faint",
            )}
            aria-hidden
          />
          <span className="min-w-0">
            {/* The name stays legible in every state. It was covered by the
                failure notice, and two failed uploads then looked identical —
                with no way to tell which file to remove. */}
            <span className="block truncate text-[0.8125rem] text-legend">
              {item.name}
            </span>
            {item.status === "uploading" && (
              <span className="legend block">{t("composer.uploading")}</span>
            )}
            {item.status === "error" && (
              <span
                className="legend block !text-red-ink"
                data-testid="composer-attachment-error"
              >
                {t("composer.uploadFailed")}
              </span>
            )}
          </span>
        </div>
      )}

      {item.isImage && item.status === "uploading" && (
        <span className="absolute inset-0 flex items-center justify-center">
          <Loader2
            size={15}
            className="animate-spin text-legend"
            aria-label={t("composer.uploading")}
          />
        </span>
      )}
      {item.isImage && item.status === "error" && (
        // A badge over the dimmed picture rather than instead of it: which
        // image failed is the first thing to answer, and only the picture
        // itself says that.
        <span
          className="absolute inset-x-0 bottom-0 truncate rounded-b-[var(--radius-control)] bg-red-quiet px-1 py-0.5 text-center text-[0.5625rem] leading-tight text-red-ink"
          data-testid="composer-attachment-error"
        >
          {t("composer.uploadFailed")}
        </span>
      )}

      <button
        type="button"
        className="key key-blank absolute -right-1.5 -top-1.5 !h-5 !w-5 !p-0"
        onClick={() => onRemove(item.localId)}
        title={t("composer.removeAttachment")}
        aria-label={t("composer.removeAttachment")}
        data-testid="composer-attachment-remove"
      >
        <X size={11} aria-hidden />
      </button>
    </div>
  );
}
