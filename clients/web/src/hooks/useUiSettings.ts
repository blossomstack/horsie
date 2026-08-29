import { useSyncExternalStore } from "react";

/** The transcript display switches. `key` names both the stored value and the
 * catalogue entry the words come from, so adding a toggle is an entry here
 * plus two strings per language — no new component code. */
export interface SettingDef {
  key: "showThinking";
  default: boolean;
}

export const SETTINGS: SettingDef[] = [{ key: "showThinking", default: false }];

const STORAGE_KEY = "horsie-ui-settings";

function loadOverrides(): Record<string, boolean> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return raw ? (JSON.parse(raw) as Record<string, boolean>) : {};
  } catch {
    return {};
  }
}

function computeValues(): Record<string, boolean> {
  const overrides = loadOverrides();
  const values: Record<string, boolean> = {};
  for (const def of SETTINGS) values[def.key] = overrides[def.key] ?? def.default;
  return values;
}

// Module-level store shared by every `useUiSettings()` call site in this tab
// (e.g. `SettingsMenu` and `SessionView` each call it independently) — a
// plain per-call `useState` would give each caller its own disconnected
// copy, so toggling in one place would never be visible anywhere else
// without a reload. `useSyncExternalStore` keeps every caller in sync
// without lifting state or wrapping the tree in a context provider.
let values = computeValues();
const listeners = new Set<() => void>();

function setValues(next: Record<string, boolean>) {
  values = next;
  localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
  listeners.forEach((listener) => listener());
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function getSnapshot(): Record<string, boolean> {
  return values;
}

export function useUiSettings(): {
  values: Record<string, boolean>;
  toggle: (key: string) => void;
} {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot);
  const toggle = (key: string) => setValues({ ...values, [key]: !values[key] });
  return { values: snapshot, toggle };
}
