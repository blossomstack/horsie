import { Info } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { SessionDetail } from "../api/types";
import { SessionConfigBar } from "./SessionConfigBar";

/**
 * What this session was launched with, one press away.
 *
 * These are settled facts, fixed for the session's lifetime — you read them
 * when something surprises you, not on every turn. Keeping them on the header
 * strip cost a whole second row of chrome above every transcript, so they live
 * behind an info key instead.
 */
export function SessionInfoMenu({ detail }: { detail: SessionDetail }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div className="relative" ref={ref}>
      <button
        className="key-icon"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        aria-label="Session details"
        title="Model, runtime, and everything else this session was launched with"
        data-testid="session-info-button"
      >
        <Info size={15} aria-hidden />
      </button>
      {open && (
        <div
          className="panel absolute right-0 top-full z-20 mt-2 w-[20rem] p-3.5 shadow-[var(--panel-lift)]"
          data-testid="session-info-panel"
        >
          <p className="legend mb-3 !text-dim">Launched with</p>
          <SessionConfigBar mode="locked" detail={detail} />
        </div>
      )}
    </div>
  );
}
