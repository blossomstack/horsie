import { useCallback, useSyncExternalStore } from "react";

/** What the user chose. `system` follows the OS and keeps following it. */
export type ThemeChoice = "light" | "dark" | "system";
/** What is actually painted — `system` is resolved away before this. */
export type Mode = "light" | "dark";
/** Which of the four worlds. Graphite is the default and carries NO
 * attribute, so `index.css` keeps the specificity it was written against. */
export type Skin = "paper" | "signal";
/** How big the interface is drawn. Scales every rem in the build, so the
 * spacing grows with the type instead of the type outgrowing its slots. */
export type TextSize = "compact" | "default" | "large";

/** The ids only — a world's name and blurb are words, so they live in the
 * catalogue and are looked up where they are drawn. */
export const TEXT_SIZES: TextSize[] = ["compact", "default", "large"];

export const SKINS: Skin[] = ["paper", "signal"];

const THEME_KEY = "horsie-theme";
const SKIN_KEY = "horsie-skin";
const TEXT_SIZE_KEY = "horsie-text-size";

const isChoice = (v: unknown): v is ThemeChoice =>
  v === "light" || v === "dark" || v === "system";
const isTextSize = (v: unknown): v is TextSize =>
  v === "compact" || v === "default" || v === "large";

function readChoice(): ThemeChoice {
  try {
    const raw = localStorage.getItem(THEME_KEY);
    // "light"/"dark" are exactly what earlier versions wrote to this key, so
    // an existing preference survives rather than resetting to system.
    return isChoice(raw) ? raw : "system";
  } catch {
    return "system";
  }
}

function readSkin(): Skin {
  try {
    const raw = localStorage.getItem(SKIN_KEY);
    return SKINS.some((s) => s === raw) ? (raw as Skin) : "paper";
  } catch {
    return "paper";
  }
}

function readTextSize(): TextSize {
  try {
    const raw = localStorage.getItem(TEXT_SIZE_KEY);
    return isTextSize(raw) ? raw : "default";
  } catch {
    return "default";
  }
}

const prefersLight = () =>
  typeof window !== "undefined" &&
  window.matchMedia("(prefers-color-scheme: light)").matches;

export const resolveMode = (choice: ThemeChoice): Mode =>
  choice === "system" ? (prefersLight() ? "light" : "dark") : choice;

/**
 * Theme state, shared across every caller in the tab.
 *
 * A module-level store rather than per-call `useState`, for the same reason
 * `useUiSettings` uses one: the rail's toggle and the Appearance page both
 * read this, and two disconnected copies would let one drift from the other
 * until a reload.
 */
let choice = readChoice();
let skin = readSkin();
let textSize = readTextSize();
const listeners = new Set<() => void>();

let snapshot = { choice, skin, textSize, mode: resolveMode(choice) };
const refresh = () => {
  snapshot = { choice, skin, textSize, mode: resolveMode(choice) };
};
const emit = () => {
  refresh();
  listeners.forEach((l) => l());
};

function apply() {
  const root = document.documentElement;
  root.dataset.theme = resolveMode(choice);
  // Paper is the default and deliberately carries no attribute, so every
  // selector in index.css keeps the specificity it was written against.
  if (skin === "paper") delete root.dataset.skin;
  else root.dataset.skin = skin;
  // Same convention: the shipped density carries no attribute, so `--text-root`
  // falls through to its default rather than being restated in two places.
  if (textSize === "default") delete root.dataset.textSize;
  else root.dataset.textSize = textSize;
}

// A `system` choice keeps tracking the OS after first paint rather than
// sampling it once at load.
if (typeof window !== "undefined") {
  window
    .matchMedia("(prefers-color-scheme: light)")
    .addEventListener("change", () => {
      if (choice === "system") {
        apply();
        emit();
      }
    });
}

function setChoice(next: ThemeChoice) {
  choice = next;
  try {
    localStorage.setItem(THEME_KEY, next);
  } catch {
    /* private mode: the in-memory value still applies for this tab */
  }
  apply();
  emit();
}

function setSkin(next: Skin) {
  skin = next;
  try {
    localStorage.setItem(SKIN_KEY, next);
  } catch {
    /* as above */
  }
  apply();
  emit();
}

function setTextSize(next: TextSize) {
  textSize = next;
  try {
    localStorage.setItem(TEXT_SIZE_KEY, next);
  } catch {
    /* as above */
  }
  apply();
  emit();
}

const subscribe = (l: () => void) => {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
};
const getSnapshot = () => snapshot;

// The inline script in index.html already set the attributes before first
// paint; this re-asserts them for the SPA's lifetime. Both worlds share the
// same two faces, so nothing is fetched on a switch.
if (typeof document !== "undefined") apply();

export function useTheme(): {
  choice: ThemeChoice;
  mode: Mode;
  skin: Skin;
  textSize: TextSize;
  setChoice: (c: ThemeChoice) => void;
  setSkin: (s: Skin) => void;
  setTextSize: (t: TextSize) => void;
  /** Flip light/dark, leaving `system` behind — a deliberate click means the
   * user wants a specific one from here on. */
  toggle: () => void;
} {
  const s = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  const toggle = useCallback(() => {
    setChoice(resolveMode(choice) === "dark" ? "light" : "dark");
  }, []);
  return {
    choice: s.choice,
    mode: s.mode,
    skin: s.skin,
    textSize: s.textSize,
    setChoice,
    setSkin,
    setTextSize,
    toggle,
  };
}
