import { MoreHorizontal } from "lucide-react";
import {
  createContext,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { useReturnFocus } from "../hooks/useReturnFocus";
import { cn } from "../lib/cn";

const CloseContext = createContext<() => void>(() => {});

/** A minimal `...` dropdown: no dependency, skin-native. The panel anchors to
 * the trigger's right edge and closes on select, Escape, or outside click. */
export function Menu({
  label,
  testId,
  children,
}: {
  label: string;
  testId?: string;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);

  // Selecting an item unmounts the button the keyboard is standing on. In a
  // session row that is 41 rows deep in the rail, losing the place means
  // tabbing back to it from the top of the document.
  useReturnFocus(open, trigger);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    const onPointer = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("pointerdown", onPointer);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("pointerdown", onPointer);
    };
  }, [open]);

  return (
    <div className="relative" ref={root}>
      <button
        ref={trigger}
        type="button"
        className="key-icon !h-6 !w-6"
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        data-testid={testId}
        onClick={(e) => {
          e.preventDefault();
          e.stopPropagation();
          setOpen((v) => !v);
        }}
      >
        <MoreHorizontal size={14} aria-hidden />
      </button>
      {open && (
        <div
          role="menu"
          className="absolute right-0 top-full z-50 mt-1 min-w-36 rounded-[var(--radius-control)] border bg-panel py-1 shadow-lg"
          onClick={(e) => {
            e.preventDefault();
            e.stopPropagation();
          }}
        >
          <CloseContext.Provider value={() => setOpen(false)}>
            {children}
          </CloseContext.Provider>
        </div>
      )}
    </div>
  );
}

export function MenuItem({
  onSelect,
  danger,
  testId,
  keepOpen,
  children,
}: {
  onSelect: () => void;
  danger?: boolean;
  testId?: string;
  /** Leave the menu open after selecting. For a checklist, where editing two
   * entries is one edit and not two trips back through the trigger. */
  keepOpen?: boolean;
  children: ReactNode;
}) {
  const close = useContext(CloseContext);
  return (
    <button
      type="button"
      role="menuitem"
      data-testid={testId}
      className={cn(
        "block w-full px-3 py-1.5 text-left text-[13px] transition-colors hover:bg-raised",
        danger ? "text-red-ink" : "text-legend",
      )}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        if (!keepOpen) close();
        onSelect();
      }}
    >
      {children}
    </button>
  );
}
