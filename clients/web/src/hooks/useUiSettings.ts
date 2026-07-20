import { useEffect, useState } from "react";

export interface SettingDef {
  key: string;
  label: string;
  description: string;
  default: boolean;
}

/** Extensible list of boolean display settings shown in `SettingsMenu` —
 * add an entry here to add a new toggle, no new component code needed. */
export const SETTINGS: SettingDef[] = [
  {
    key: "showThinking",
    label: "Show thinking",
    description: "Reveal the model's reasoning steps in the transcript.",
    default: false,
  },
];

const STORAGE_KEY = "horsie-ui-settings";

function loadOverrides(): Record<string, boolean> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return raw ? (JSON.parse(raw) as Record<string, boolean>) : {};
  } catch {
    return {};
  }
}

function initialValues(): Record<string, boolean> {
  const overrides = loadOverrides();
  const values: Record<string, boolean> = {};
  for (const def of SETTINGS) values[def.key] = overrides[def.key] ?? def.default;
  return values;
}

export function useUiSettings(): {
  values: Record<string, boolean>;
  toggle: (key: string) => void;
} {
  const [values, setValues] = useState<Record<string, boolean>>(initialValues);

  useEffect(() => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(values));
  }, [values]);

  const toggle = (key: string) => setValues((v) => ({ ...v, [key]: !v[key] }));

  return { values, toggle };
}
