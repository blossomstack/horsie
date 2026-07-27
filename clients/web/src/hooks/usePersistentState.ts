import { useState } from "react";

export interface PersistentStateOptions<T> {
  /** Map the value to a JSON-serializable shape (default: identity). */
  serialize?: (value: T) => unknown;
  /**
   * Validate a parsed JSON value; return `undefined` to reject it (wrong
   * version, bad shape) and fall back to `initial`.
   */
  deserialize?: (raw: unknown) => T | undefined;
}

/**
 * `useState` mirrored to localStorage under `key`. Hydrates lazily on first
 * render; every set writes through. Missing key, corrupt JSON, or a rejected
 * deserialize all fall back to `initial`. No cross-tab sync — last write
 * wins, same as `useUiSettings`.
 */
export function usePersistentState<T>(
  key: string,
  initial: T,
  options: PersistentStateOptions<T> = {},
): [T, (next: T) => void] {
  const [value, setValue] = useState<T>(() => {
    try {
      const rawJson = localStorage.getItem(key);
      if (rawJson === null) return initial;
      const parsed: unknown = JSON.parse(rawJson);
      const hydrated = options.deserialize ? options.deserialize(parsed) : (parsed as T);
      return hydrated === undefined ? initial : hydrated;
    } catch {
      return initial;
    }
  });

  const set = (next: T) => {
    setValue(next);
    try {
      localStorage.setItem(
        key,
        JSON.stringify(options.serialize ? options.serialize(next) : next),
      );
    } catch {
      // Storage full or unavailable — keep the in-memory state; a lost
      // preference must never break the UI.
    }
  };

  return [value, set];
}
