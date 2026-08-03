/**
 * Copy helpers for a transcript turn.
 *
 * Two flavours, because the source and the rendering are different useful
 * things: the markdown a model wrote is what you paste back into an issue or
 * another prompt, and the rendered text is what you paste into a message to a
 * colleague.
 */

/** `navigator.clipboard` is unavailable on a plain-HTTP origin, which is the
 * normal case for a horsie server on a LAN. Falls back to the legacy path so
 * copying still works there rather than silently doing nothing. */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    /* fall through to the legacy path */
  }
  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    // Off-screen rather than `display: none` — a hidden element cannot be
    // selected, and `execCommand("copy")` copies the selection.
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    ta.style.pointerEvents = "none";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    document.body.removeChild(ta);
    return ok;
  } catch {
    return false;
  }
}

/**
 * The turn as a reader sees it.
 *
 * Deliberately taken from the rendered DOM rather than by parsing the markdown
 * a second time: whatever the renderer decided — a table flattened to rows, a
 * fenced block's contents, an entity resolved — is what the user is looking
 * at, and a separate parse would be a second opinion that can disagree with
 * the screen. `innerText` is used over `textContent` because it is
 * layout-aware, so block elements keep the line breaks that make the result
 * readable; `textContent` would run a heading straight into the next
 * paragraph. jsdom implements only the latter, hence the fallback.
 */
export function renderedTextOf(el: HTMLElement | null): string {
  if (!el) return "";
  // Only the prose. A turn's DOM also holds tool-call rows, work-group
  // summaries and ask cards; taking the container's text wholesale meant
  // "copy as plain text" returned something the markdown button never would.
  const parts = el.querySelectorAll<HTMLElement>("[data-prose-segment]");
  const nodes = parts.length > 0 ? Array.from(parts) : [el];
  return nodes
    .map((n) => n.innerText ?? n.textContent ?? "")
    .join("\n\n")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}
