import { Menu } from "lucide-react";
import {
  createContext,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from "react";

/** Below `md` the session rail is a drawer, not a column — at 390px it would
 * otherwise eat two thirds of the viewport and leave the transcript unusable.
 * Pages render `<RailToggle/>` in their own header so the control sits where
 * the eye already is. */
const RailContext = createContext<{
  open: boolean;
  setOpen: (v: boolean) => void;
}>({ open: false, setOpen: () => {} });

export function RailProvider({ children }: { children: ReactNode }) {
  const [open, setOpen] = useState(false);
  return (
    <RailContext.Provider value={{ open, setOpen }}>
      {children}
    </RailContext.Provider>
  );
}

export function useRail() {
  return useContext(RailContext);
}

/** Opens the rail on small screens. Invisible once the rail is a real column. */
export function RailToggle() {
  const { setOpen } = useRail();
  return (
    <button
      className="key-icon -ml-1.5 shrink-0 md:hidden"
      onClick={() => setOpen(true)}
      aria-label="Show sessions"
      title="Show sessions"
      data-testid="rail-toggle"
    >
      <Menu size={16} aria-hidden />
    </button>
  );
}

/** Closes the drawer on route change and on Escape. */
export function useRailAutoClose(pathname: string) {
  const { open, setOpen } = useRail();
  useEffect(() => {
    setOpen(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pathname]);
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, setOpen]);
}
