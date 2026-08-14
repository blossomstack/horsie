/** Laying a session out along a horizontal axis.
 *
 * The axis is wall-clock order with the dead air taken out. A session is mostly
 * waiting — for a person to come back, for a long tool call — and an honest
 * linear axis spends almost all of its width on nothing. So: real elapsed time
 * between entries, up to a minute; past that, a fixed gutter labelled with what
 * it swallowed.
 *
 * The consequence, and the reason this hands back a `toX` function rather than
 * a multiplier: the drawn axis is monotone but NOT linear in time, so no caller
 * can turn a timestamp into a pixel by arithmetic. Every off-lane moment — a
 * subagent's spawn, a fork's branch point — goes through `toX`.
 */

/** A gap longer than this is dead air, not part of the work. */
export const GAP_THRESHOLD_MS = 60_000;
/** What a collapsed gap is drawn at, however long it really was. */
export const GAP_PX = 20;
/** Small enough to read as brief, big enough to still be a click target. */
export const MIN_BAR_PX = 6;
/** One forty-minute tool call must not push the rest of the session off screen. */
export const MAX_BAR_PX = 320;
/** Roughly three pane-widths of drawn session. */
export const TARGET_PX = 2400;
export const MIN_SCALE = 0.0005;
export const MAX_SCALE = 0.02;

export interface Span {
  startMs: number;
  endMs: number;
}

export interface Scale {
  /** Where a moment lands, in pixels from the left edge. Clamped at both ends. */
  toX(ms: number): number;
  width: number;
  gaps: { x: number; elapsedMs: number }[];
}

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

/**
 * Build the time-to-pixel map from the spans that will be drawn on the main lane.
 *
 * Spans are taken in the order given and assumed non-overlapping, which is what
 * a transcript produces: an assistant message and the tools it issued are
 * consecutive, and parallel tool calls share a start but are laid out in issue
 * order.
 */
export function buildScale(spans: Span[]): Scale {
  if (spans.length === 0) {
    return { toX: () => 0, width: 0, gaps: [] };
  }

  // Scaled on *active* time — elapsed minus everything that will collapse —
  // rather than on total elapsed, which would squeeze a session with one
  // overnight gap in it down to a smudge.
  let activeMs = 0;
  for (let i = 0; i < spans.length; i++) {
    activeMs += Math.max(0, spans[i].endMs - spans[i].startMs);
    if (i > 0) {
      const gap = spans[i].startMs - spans[i - 1].endMs;
      if (gap > 0 && gap <= GAP_THRESHOLD_MS) activeMs += gap;
    }
  }
  const scale = activeMs > 0 ? clamp(TARGET_PX / activeMs, MIN_SCALE, MAX_SCALE) : MIN_SCALE;

  // Breakpoints: (ms, px) pairs, both increasing. Between two consecutive
  // points the map is linear, which is what makes `toX` one interpolation
  // rather than a special case per kind of interval.
  const ms: number[] = [];
  const px: number[] = [];
  const gaps: { x: number; elapsedMs: number }[] = [];
  let x = 0;

  // A zero-duration span — a user message — would otherwise put two points at
  // the same instant with different pixels, and the interpolation below would
  // divide by zero. The later pixel wins, so the bar still has its width.
  const push = (atMs: number, atPx: number) => {
    if (ms.length > 0 && atMs === ms[ms.length - 1]) {
      px[px.length - 1] = atPx;
      return;
    }
    ms.push(atMs);
    px.push(atPx);
  };

  push(spans[0].startMs, 0);
  for (let i = 0; i < spans.length; i++) {
    if (i > 0) {
      const gap = spans[i].startMs - spans[i - 1].endMs;
      if (gap > GAP_THRESHOLD_MS) {
        gaps.push({ x, elapsedMs: gap });
        x += GAP_PX;
      } else if (gap > 0) {
        x += gap * scale;
      }
      push(spans[i].startMs, x);
    }
    const duration = Math.max(0, spans[i].endMs - spans[i].startMs);
    // Clamped, which is the one place the drawing stops being literally true.
    // A bar at the cap is marked in the UI and its tooltip carries the real
    // number; a zero-duration span still gets MIN_BAR_PX so it stays clickable.
    x += clamp(duration * scale, MIN_BAR_PX, MAX_BAR_PX);
    push(spans[i].endMs, x);
  }

  const width = x;
  const toX = (at: number): number => {
    if (at <= ms[0]) return 0;
    if (at >= ms[ms.length - 1]) return width;
    // A linear scan: a session has hundreds of breakpoints, not millions, and
    // this runs once per off-lane agent rather than once per frame.
    for (let i = 1; i < ms.length; i++) {
      if (at <= ms[i]) {
        const spanMs = ms[i] - ms[i - 1];
        const t = spanMs === 0 ? 0 : (at - ms[i - 1]) / spanMs;
        return px[i - 1] + t * (px[i] - px[i - 1]);
      }
    }
    return width;
  };

  return { toX, width, gaps };
}
