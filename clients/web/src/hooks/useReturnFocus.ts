import { useEffect, useRef, type RefObject } from "react";

/**
 * Hand focus back to the control that opened a transient surface.
 *
 * Every popup in this app unmounts its panel while the keyboard may still be
 * inside it, and a browser answers a removed `activeElement` by dropping focus
 * on `<body>`. The next Tab then restarts at the top of the document — which
 * here means walking the whole session rail again — so a keyboard user pays
 * for opening a menu.
 *
 * The restore is deferred by one task on purpose. An outside pointerdown
 * closes the surface *before* the browser has run the click's default action,
 * so deciding synchronously either fights a legitimate focus move onto the
 * control that was clicked, or is undone by the blur that clicking plain
 * content causes. One task later the browser has settled and the only question
 * left is whether focus was stranded — which is the only case this claims.
 */
export function useReturnFocus(
  open: boolean,
  trigger: RefObject<HTMLElement | null>,
) {
  const wasOpen = useRef(false);
  useEffect(() => {
    if (open) {
      wasOpen.current = true;
      return;
    }
    // Never on mount: a control that has not been opened yet has no focus to
    // take back, and grabbing it would move the caret out of whatever the page
    // legitimately focused.
    if (!wasOpen.current) return;
    wasOpen.current = false;
    const el = trigger.current;
    if (!el) return;
    const timer = setTimeout(() => {
      const active = document.activeElement;
      const stranded =
        !active ||
        active === document.body ||
        !active.isConnected ||
        el.contains(active);
      if (!stranded) return;
      el.focus();
    });
    return () => clearTimeout(timer);
  }, [open, trigger]);
}
