import { Pencil, X } from "lucide-react";
import {
  createContext,
  useContext,
  useEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type MouseEvent,
  type ReactNode,
  type RefObject,
} from "react";
import { useTranslation } from "react-i18next";

/** One excerpt the user has attached a comment to, before it becomes a message. */
export interface TranscriptComment {
  id: string;
  anchorId: string;
  quote: string;
  comment: string;
}

export interface TranscriptCommentPromptLabels {
  intro: string;
  excerpt: string;
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
  labels: TranscriptCommentPromptLabels,
): string {
  const sections = comments.map((item, index) => {
    const quote = item.quote
      .split("\n")
      .map((line) => `> ${line}`)
      .join("\n");
    return `${labels.excerpt} ${index + 1}:\n${quote}\n\n${labels.comment}:\n${item.comment}`;
  });
  return `${labels.intro}\n\n${sections.join("\n\n")}`;
}

interface CommentDraft {
  anchorId: string;
  quote: string;
}

interface CommentContextValue {
  comments: readonly TranscriptComment[];
  draft: CommentDraft | null;
  setDraft: (draft: CommentDraft | null) => void;
  editing: { id: string; original: string } | null;
  setEditing: (editing: { id: string; original: string } | null) => void;
  onAdd: (comment: TranscriptComment) => void;
  onUpdate: (id: string, comment: string) => void;
  onRemove: (id: string) => void;
}

const CommentContext = createContext<CommentContextValue | null>(null);
let commentSequence = 0;

function elementForNode(node: Node): Element | null {
  return node.nodeType === Node.ELEMENT_NODE
    ? (node as Element)
    : node.parentElement;
}

/**
 * Read a native browser selection only when both ends belong to one settled
 * transcript turn. A cross-turn quote has no single place to keep its comment.
 */
export function transcriptSelection(
  root: HTMLElement,
  selection: Selection | null,
): CommentDraft | null {
  if (!selection || selection.isCollapsed || selection.rangeCount === 0) return null;
  const range = selection.getRangeAt(0);
  const start = elementForNode(range.startContainer);
  const end = elementForNode(range.endContainer);
  if (!start || !end || !root.contains(start) || !root.contains(end)) return null;
  if (start.closest("[data-comment-ui]") || end.closest("[data-comment-ui]")) return null;
  if (
    start.closest("[data-comment-disabled]") ||
    end.closest("[data-comment-disabled]")
  )
    return null;
  const startTurn = start.closest<HTMLElement>("[data-comment-anchor]");
  const endTurn = end.closest<HTMLElement>("[data-comment-anchor]");
  if (!startTurn || startTurn !== endTurn) return null;
  const anchorId = startTurn.dataset.commentAnchor;
  const quote = selection.toString().trim();
  return anchorId && quote ? { anchorId, quote } : null;
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
  const [draft, setDraft] = useState<CommentDraft | null>(null);
  const [editing, setEditing] = useState<{
    id: string;
    original: string;
  } | null>(null);
  const selectionTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const pointerSelecting = useRef(false);
  const enabled = commenting !== undefined;

  const readSelection = () => {
    if (selectionTimer.current) clearTimeout(selectionTimer.current);
    const root = rootRef.current;
    if (!root || !commenting) return;
    const next = transcriptSelection(root, window.getSelection());
    if (next) setDraft(next);
  };

  // Keyboard and touch selections have no mouse-up event. Waiting until the
  // selection rests keeps focus from jumping to the comment field mid-drag.
  useEffect(() => {
    if (!enabled) return;
    const onSelectionChange = () => {
      if (pointerSelecting.current) return;
      if (selectionTimer.current) clearTimeout(selectionTimer.current);
      selectionTimer.current = setTimeout(() => {
        const root = rootRef.current;
        const next = root
          ? transcriptSelection(root, window.getSelection())
          : null;
        if (next) setDraft(next);
      }, 250);
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

  return (
    <CommentContext.Provider
      value={{
        comments: commenting.comments,
        draft,
        setDraft,
        editing,
        setEditing,
        onAdd: commenting.onAdd,
        onUpdate: commenting.onUpdate,
        onRemove: commenting.onRemove,
      }}
    >
      {body}
    </CommentContext.Provider>
  );
}

function CommentEditor({
  initialValue = "",
  submitLabel,
  ariaLabel,
  onChange,
  onSubmit,
  onCancel,
}: {
  initialValue?: string;
  submitLabel: string;
  ariaLabel?: string;
  onChange?: (value: string) => void;
  onSubmit: (value: string) => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  const [value, setValue] = useState(initialValue);
  const submit = () => {
    const comment = value.trim();
    if (comment) onSubmit(comment);
  };
  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Escape") {
      event.preventDefault();
      onCancel();
    } else if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      submit();
    }
  };
  const keepSelectionLocal = (event: MouseEvent) => event.stopPropagation();

  return (
    <div data-comment-ui="" onMouseUp={keepSelectionLocal}>
      <textarea
        autoFocus
        rows={2}
        value={value}
        onChange={(event) => {
          setValue(event.target.value);
          onChange?.(event.target.value);
        }}
        onKeyDown={onKeyDown}
        aria-label={ariaLabel ?? t("transcript.commentPlaceholder")}
        placeholder={t("transcript.commentPlaceholder")}
        className="screen w-full resize-y px-2.5 py-2 text-sm leading-relaxed text-legend outline-none transition-shadow placeholder:text-faint focus:shadow-[0_0_0_2px_var(--focus-ring)]"
      />
      <div className="mt-2 flex justify-end gap-1.5">
        <button type="button" className="key key-flat key-sm" onClick={onCancel}>
          {t("common.cancel")}
        </button>
        <button
          type="button"
          className="key key-go key-sm"
          disabled={!value.trim()}
          onClick={submit}
        >
          {submitLabel}
        </button>
      </div>
    </div>
  );
}

function SavedComment({ item }: { item: TranscriptComment }) {
  const { t } = useTranslation();
  const context = useContext(CommentContext);
  if (!context) return null;
  const editingOriginal =
    context.editing?.id === item.id ? context.editing.original : null;
  const editing = editingOriginal !== null;

  return (
    <div
      className="rounded-[var(--radius-control)] bg-raised px-3 py-2.5"
      data-testid="transcript-comment"
      data-comment-ui=""
    >
      {editing ? (
        <>
          <p className="mb-2 whitespace-pre-wrap text-xs leading-relaxed text-faint">
            “{item.quote}”
          </p>
          <CommentEditor
            initialValue={item.comment}
            submitLabel={t("common.save")}
            ariaLabel={t("transcript.editComment")}
            onChange={(comment) => context.onUpdate(item.id, comment)}
            onCancel={() => {
              context.onUpdate(item.id, editingOriginal ?? item.comment);
              context.setEditing(null);
            }}
            onSubmit={(comment) => {
              context.onUpdate(item.id, comment);
              context.setEditing(null);
            }}
          />
        </>
      ) : (
        <>
          <div className="flex items-start gap-2">
            <p className="min-w-0 flex-1 whitespace-pre-wrap text-sm leading-relaxed text-legend">
              {item.comment}
            </p>
            <div className="flex shrink-0 gap-0.5">
              <button
                type="button"
                className="key-icon !h-6 !w-6"
                onClick={() =>
                  context.setEditing({ id: item.id, original: item.comment })
                }
                title={t("transcript.editComment")}
                aria-label={t("transcript.editComment")}
              >
                <Pencil size={13} aria-hidden />
              </button>
              <button
                type="button"
                className="key-icon !h-6 !w-6"
                onClick={() => {
                  context.setEditing(null);
                  context.onRemove(item.id);
                }}
                title={t("transcript.removeComment")}
                aria-label={t("transcript.removeComment")}
              >
                <X size={14} aria-hidden />
              </button>
            </div>
          </div>
          <p className="mt-1.5 whitespace-pre-wrap text-xs leading-relaxed text-faint">
            “{item.quote}”
          </p>
        </>
      )}
    </div>
  );
}
/** Comments and the active comment field stay attached to the quoted turn. */
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
  const draft =
    context.draft && anchors.has(context.draft.anchorId)
      ? context.draft
      : null;
  if (comments.length === 0 && !draft) return null;

  return (
    <div className="space-y-2 pt-1" data-testid="transcript-comments">
      {comments.map((item) => (
        <SavedComment key={item.id} item={item} />
      ))}
      {draft && (
        <div
          className="rounded-[var(--radius-control)] bg-raised px-3 py-2.5"
          data-testid="transcript-comment-draft"
          data-comment-ui=""
        >
          <p className="mb-2 whitespace-pre-wrap text-xs leading-relaxed text-faint">
            “{draft.quote}”
          </p>
          <CommentEditor
            submitLabel={t("transcript.addComment")}
            onCancel={() => {
              context.setDraft(null);
              window.getSelection()?.removeAllRanges();
            }}
            onSubmit={(comment) => {
              context.onAdd({
                id: `transcript-comment-${++commentSequence}`,
                anchorId: draft.anchorId,
                quote: draft.quote,
                comment,
              });
              context.setDraft(null);
              window.getSelection()?.removeAllRanges();
            }}
          />
        </div>
      )}
    </div>
  );
}
