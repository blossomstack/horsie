import { useCallback, useState, type UIEvent } from "react";

/**
 * Whether a scroll region has anything scrolled up under the bar above it.
 *
 * Drives the one shadow a header bar is allowed. The bar carries no border:
 * a permanent rule under a header states a boundary that is only true half
 * the time, because when the content sits at the top there is nothing beneath
 * the bar to separate it from. This reports exactly when there is.
 *
 * A `scroll` handler rather than an `IntersectionObserver` sentinel, because
 * the header and its scroll region are siblings in every caller — the state
 * has nowhere to travel — and because a sentinel needs an element inside the
 * scroller that exists for no other reason. The handler is passive by
 * React's own default and only re-renders when the boolean actually flips,
 * so a long transcript does not re-render per frame while it scrolls.
 */
export function useScrolledUnder(): {
  scrolled: boolean;
  /** Spread onto the scrolling element. */
  onScroll: (e: UIEvent<HTMLElement>) => void;
  /** Spread onto the bar. Absent rather than `"false"` so the attribute only
   * exists while it means something. */
  barProps: { "data-scrolled"?: "true" };
} {
  const [scrolled, setScrolled] = useState(false);
  const onScroll = useCallback((e: UIEvent<HTMLElement>) => {
    // A couple of pixels of slack: a trackpad resting at the top can report
    // a sub-pixel scrollTop, and a shadow that flickers on and off as you
    // breathe on the wheel is worse than no shadow.
    const next = e.currentTarget.scrollTop > 2;
    setScrolled((prev) => (prev === next ? prev : next));
  }, []);
  return { scrolled, onScroll, barProps: scrolled ? { "data-scrolled": "true" } : {} };
}
