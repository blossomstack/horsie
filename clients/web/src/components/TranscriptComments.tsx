import { X } from "lucide-react";
import {
  createContext,
  useContext,
  useEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type ReactNode,
  type RefObject,
} from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { cn } from "../lib/cn";

/** One excerpt the user has attached a comment to, before it becomes a message. */
export interface TranscriptComment {
  id: string;
  anchorId: string;
  quote: string;
  comment: string;
}

export interface TranscriptCommenting {
  comments: readonly TranscriptComment[];
  onAdd: (comment: TranscriptComment) => void;
  onUpdate: (id: string, comment: string) => void;
  onRemove: (id: string) => void;
}

export function formatTranscriptComments(
  comments: readonly TranscriptComment[],
): string {
  return comments
    .map((item) => {
      const quote = item.quote
        .split("\n")
        .map((line) => `> ${line}`)
        .join("\n");
      return `${quote}\n${item.comment}`;
    })
    .join("\n\n");
}

interface SelectedExcerpt {
  anchorId: string;
  quote: string;
}

interface PanelPosition {
  left: number;
  top: number;
}

interface ActiveComment {
  id: string;
  original: string;
  isNew: boolean;
  position: PanelPosition;
}

interface CommentContextValue extends TranscriptCommenting {
  active: ActiveComment | null;
  setActive: (active: ActiveComment | null) => void;
}

const CommentContext = createContext<CommentContextValue | null>(null);
let commentSequence = 0;

function elementForNode(node: Node): Element | null {
  return node.nodeType === Node.ELEMENT_NODE
    ? (node as Element)
    : node.parentElement;
}

/** A comment belongs to one settled message, never to chrome or a live tail. */
export function transcriptSelection(
  root: HTMLElement,
  selection: Selection | null,
): SelectedExcerpt | null {
  if (!selection || selection.isCollapsed || selection.rangeCount === 0) return null;
  const range = selection.getRangeAt(0);
  const start = elementForNode(range.startContainer);
  const end = elementForNode(range.endContainer);
  if (!start || !end || !root.contains(start) || !root.contains(end)) return null;
  if (start.closest("[data-comment-ui]") || end.closest("[data-comment-ui]")) return null;
  if (
    start.closest("[data-comment-disabled]") ||
    end.closest("[data-comment-disabled]")
  ) {
    return null;
  }
  const startTurn = start.closest<HTMLElement>("[data-comment-anchor]");
  const endTurn = end.closest<HTMLElement>("[data-comment-anchor]");
  if (!startTurn || startTurn !== endTurn) return null;
  const anchorId = startTurn.dataset.commentAnchor;
  const quote = selection.toString().trim();
  return anchorId && quote ? { anchorId, quote } : null;
}

const PANEL_WIDTH = 352;
const PANEL_HEIGHT = 280;
const PANEL_GAP = 8;

function positionPanel(rect: Pick<DOMRect, "bottom" | "left" | "top">): PanelPosition {
  const width = Math.min(PANEL_WIDTH, window.innerWidth - PANEL_GAP * 2);
  const left = Math.min(
    Math.max(PANEL_GAP, rect.left),
    Math.max(PANEL_GAP, window.innerWidth - width - PANEL_GAP),
  );
  const below = rect.bottom + PANEL_GAP;
  const top =
    below + PANEL_HEIGHT <= window.innerHeight
      ? below
      : Math.max(PANEL_GAP, rect.top - PANEL_HEIGHT - PANEL_GAP);
  return { left, top };
}

function selectionPosition(selection: Selection): PanelPosition {
  const range = selection.getRangeAt(0);
  // jsdom has no range layout; browsers do. The fallback also keeps the panel
  // reachable in any engine that returns an incomplete Range implementation.
  const rect =
    typeof range.getBoundingClientRect === "function"
      ? range.getBoundingClientRect()
      : ({ top: PANEL_GAP, bottom: PANEL_GAP, left: PANEL_GAP } as DOMRect);
  return positionPanel(rect);
}

function CommentEditor({
  value,
  submitLabel,
  ariaLabel,
  onChange,
  onSubmit,
  onCancel,
  onDelete,
  onCollapse,
}: {
  value: string;
  submitLabel: string;
  ariaLabel: string;
  onChange: (value: string) => void;
  onSubmit: () => void;
  onCancel: () => void;
  onDelete?: () => void;
  onCollapse: () => void;
}) {
  const { t } = useTranslation();
  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Escape") {
      event.preventDefault();
      onCollapse();
    } else if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      if (value.trim()) onSubmit();
    }
  };

  return (
    <>
      <textarea
        autoFocus
        rows={3}
        value={value}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={onKeyDown}
        aria-label={ariaLabel}
        placeholder={t("transcript.commentPlaceholder")}
        className="screen w-full resize-y px-2.5 py-2 text-sm leading-relaxed text-legend outline-none transition-shadow placeholder:text-faint focus:shadow-[0_0_0_2px_var(--focus-ring)]"
      />
      <div className="mt-2 flex justify-end gap-1.5">
        {onDelete && (
          <button
            type="button"
            className="key key-danger key-sm mr-auto"
            onClick={onDelete}
          >
            {t("common.remove")}
          </button>
        )}
        <button type="button" className="key key-flat key-sm" onClick={onCancel}>
          {t("common.cancel")}
        </button>
        <button
          type="button"
          className="key key-go key-sm"
          disabled={!value.trim()}
          onClick={onSubmit}
        >
          {submitLabel}
        </button>
      </div>
    </>
  );
}

function FloatingCommentPanel({
  item,
  active,
  onUpdate,
  onRemove,
  onClose,
}: {
  item: TranscriptComment;
  active: ActiveComment;
  onUpdate: (id: string, comment: string) => void;
  onRemove: (id: string) => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const cancel = () => {
    if (active.isNew) onRemove(item.id);
    else onUpdate(item.id, active.original);
    onClose();
  };
  const save = () => {
    const comment = item.comment.trim();
    if (!comment) return;
    onUpdate(item.id, comment);
    onClose();
  };

  return (
    <div
      role="dialog"
      aria-label={t("transcript.commentPanelTitle")}
      data-testid="transcript-comment-panel"
      data-comment-ui=""
      className="floating fixed z-[60] w-[min(22rem,calc(100vw-1rem))] p-3 shadow-[0_12px_36px_oklch(0_0_0/0.28)]"
      style={{ left: active.position.left, top: active.position.top }}
    >
      <div className="mb-2 flex items-start gap-2">
        <p className="max-h-24 min-w-0 flex-1 overflow-y-auto whitespace-pre-wrap text-xs leading-relaxed text-faint">
          “{item.quote}”
        </p>
        <button
          type="button"
          className="key-icon !h-6 !w-6 shrink-0"
          onClick={onClose}
          title={t("transcript.collapseComment")}
          aria-label={t("transcript.collapseComment")}
        >
          <X size={14} aria-hidden />
        </button>
      </div>
      <CommentEditor
        value={item.comment}
        submitLabel={
          active.isNew ? t("transcript.addComment") : t("common.save")
        }
        ariaLabel={
          active.isNew
            ? t("transcript.commentPlaceholder")
            : t("transcript.editComment")
        }
        onChange={(comment) => onUpdate(item.id, comment)}
        onSubmit={save}
        onCancel={cancel}
        onDelete={
          active.isNew
            ? undefined
            : () => {
                onRemove(item.id);
                onClose();
              }
        }
        onCollapse={onClose}
      />
    </div>
  );
}

export function TranscriptCommentProvider({
  rootRef,
  commenting,
  className,
  children,
}: {
  rootRef: RefObject<HTMLDivElement | null>;
  commenting?: TranscriptCommenting;
  className?: string;
  children: ReactNode;
}) {
  const [active, setActive] = useState<ActiveComment | null>(null);
  const selectionTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const pointerSelecting = useRef(false);
  const commentingRef = useRef(commenting);
  commentingRef.current = commenting;
  const enabled = commenting !== undefined;

  const readSelection = () => {
    if (selectionTimer.current) clearTimeout(selectionTimer.current);
    const root = rootRef.current;
    const selection = window.getSelection();
    const current = commentingRef.current;
    if (!root || !current || !selection) return;
    const excerpt = transcriptSelection(root, selection);
    if (!excerpt) return;
    const id = `transcript-comment-${++commentSequence}`;
    current.onAdd({ id, ...excerpt, comment: "" });
    setActive({
      id,
      original: "",
      isNew: true,
      position: selectionPosition(selection),
    });
    selection.removeAllRanges();
  };

  // Keyboard and touch selections have no mouse-up event. Waiting until the
  // selection rests keeps focus from jumping to the panel mid-drag.
  useEffect(() => {
    if (!enabled) return;
    const onSelectionChange = () => {
      if (pointerSelecting.current) return;
      if (selectionTimer.current) clearTimeout(selectionTimer.current);
      selectionTimer.current = setTimeout(readSelection, 250);
    };
    document.addEventListener("selectionchange", onSelectionChange);
    return () => {
      document.removeEventListener("selectionchange", onSelectionChange);
      if (selectionTimer.current) clearTimeout(selectionTimer.current);
    };
  }, [enabled, rootRef]);

  const body = (
    <div
      ref={rootRef}
      className={className}
      onPointerDown={
        commenting
          ? () => {
              pointerSelecting.current = true;
            }
          : undefined
      }
      onPointerUp={
        commenting
          ? () => {
              pointerSelecting.current = false;
              readSelection();
            }
          : undefined
      }
      onPointerCancel={
        commenting
          ? () => {
              pointerSelecting.current = false;
            }
          : undefined
      }
      onMouseUp={commenting ? readSelection : undefined}
      data-testid={commenting ? "commentable-transcript" : undefined}
    >
      {children}
    </div>
  );
  if (!commenting) return body;

  const activeItem = active
    ? commenting.comments.find((item) => item.id === active.id)
    : undefined;
  const closeActive = () => {
    const id = active?.id;
    setActive(null);
    if (!id) return;
    setTimeout(() => {
      const markers = rootRef.current?.querySelectorAll<HTMLButtonElement>(
        "[data-comment-id]",
      );
      Array.from(markers ?? []).find(
        (marker) => marker.dataset.commentId === id,
      )?.focus();
    });
  };

  return (
    <CommentContext.Provider value={{ ...commenting, active, setActive }}>
      {body}
      {active && activeItem
        ? createPortal(
            <FloatingCommentPanel
              item={activeItem}
              active={active}
              onUpdate={commenting.onUpdate}
              onRemove={commenting.onRemove}
              onClose={closeActive}
            />,
            document.body,
          )
        : null}
    </CommentContext.Provider>
  );
}

/** Compact markers keep every comment attached without expanding every card. */
export function TranscriptTurnComments({
  anchorIds,
}: {
  anchorIds: readonly string[];
}) {
  const { t } = useTranslation();
  const context = useContext(CommentContext);
  if (!context) return null;
  const anchors = new Set(anchorIds);
  const comments = context.comments.filter((item) => anchors.has(item.anchorId));
  if (comments.length === 0) return null;

  return (
    <div
      className="absolute -top-6 right-0 z-20 flex gap-1 sm:-right-2 sm:top-0 sm:translate-x-full"
      data-testid="transcript-comment-markers"
      data-comment-ui=""
    >
      {comments.map((item) => {
        const open = context.active?.id === item.id;
        const number = context.comments.findIndex((comment) => comment.id === item.id) + 1;
        const markerLabel = t("transcript.openComment", {
          excerpt: item.quote.length > 48 ? `${item.quote.slice(0, 48)}…` : item.quote,
        });
        return (
          <button
            key={item.id}
            type="button"
            className={cn(
              "key-icon !h-6 !w-6 bg-raised text-legend shadow-[0_2px_10px_oklch(0_0_0/0.16)]",
              open && "!bg-accent-quiet !text-legend",
            )}
            aria-pressed={open}
            aria-label={
              open
                ? t("transcript.collapseComment")
                : markerLabel
            }
            title={
              open
                ? t("transcript.collapseComment")
                : markerLabel
            }
            data-testid="transcript-comment-marker"
            data-comment-id={item.id}
            onClick={(event) => {
              if (open) {
                context.setActive(null);
                return;
              }
              const rect = event.currentTarget.getBoundingClientRect();
              context.setActive({
                id: item.id,
                original: item.comment,
                isNew: item.comment.trim().length === 0,
                position: positionPanel(rect),
              });
            }}
          >
            <span className="readout text-[0.625rem]" aria-hidden>
              {number}
            </span>
          </button>
        );
      })}
    </div>
  );
}
