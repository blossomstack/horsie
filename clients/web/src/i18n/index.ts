import i18next from "i18next";
import { initReactI18next } from "react-i18next";
import { useSyncExternalStore } from "react";
import en from "./locales/en";
import zhHans from "./locales/zh-Hans";
import zhHant from "./locales/zh-Hant";

/** A language the interface is actually written in. */
export type Locale = "en" | "zh-Hans" | "zh-Hant";
/** What the user picked. `system` follows the browser and keeps following it. */
export type LocaleChoice = Locale | "system";

/** Language names are endonyms and are never translated — someone who has
 * landed in a language they cannot read needs to find their own in the list. */
export const LOCALES: { id: Locale; name: string; note: string }[] = [
  { id: "en", name: "English", note: "English" },
  { id: "zh-Hans", name: "简体中文", note: "Simplified Chinese" },
  { id: "zh-Hant", name: "繁體中文", note: "Traditional Chinese" },
];

const STORAGE_KEY = "horsie-locale";

const isLocale = (v: unknown): v is Locale =>
  LOCALES.some((l) => l.id === v);

/** Which Chinese a browser tag asks for.
 *
 * Only the script is decisive, and most browsers send a region instead of one
 * (`zh-TW`, `zh-HK`), so the region has to be read as a proxy for it. Anything
 * else under `zh` is Simplified, which is what `zh`, `zh-CN` and `zh-SG` all
 * mean in practice. */
function matchLocale(tag: string): Locale | undefined {
  const lower = tag.toLowerCase();
  if (!lower.startsWith("zh")) return lower.startsWith("en") ? "en" : undefined;
  return /hant|\b(tw|hk|mo)\b/.test(lower) ? "zh-Hant" : "zh-Hans";
}

/** The first of the browser's languages this build can speak. */
export function detectLocale(): Locale {
  if (typeof navigator === "undefined") return "en";
  const tags = navigator.languages?.length
    ? navigator.languages
    : [navigator.language];
  for (const tag of tags) {
    const hit = matchLocale(tag ?? "");
    if (hit) return hit;
  }
  return "en";
}

export const resolveLocale = (choice: LocaleChoice): Locale =>
  choice === "system" ? detectLocale() : choice;

function readChoice(): LocaleChoice {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return raw === "system" || isLocale(raw) ? raw : "system";
  } catch {
    return "system";
  }
}

// Module-level store, for the same reason `useUiSettings` has one: the picker
// in Appearance and any other reader of the choice must see one value, not a
// copy each. The rendered *strings* come from react-i18next's own context —
// this store only owns the choice, including the `system` that i18next has no
// concept of.
let choice: LocaleChoice = readChoice();
const listeners = new Set<() => void>();

function apply(locale: Locale) {
  void i18next.changeLanguage(locale);
  if (typeof document !== "undefined") {
    document.documentElement.lang = locale;
  }
}

export function setLocaleChoice(next: LocaleChoice) {
  choice = next;
  try {
    localStorage.setItem(STORAGE_KEY, next);
  } catch {
    // A browser with storage denied still switches for this tab.
  }
  apply(resolveLocale(next));
  listeners.forEach((listener) => listener());
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

const getChoice = () => choice;

export function useLocaleChoice(): {
  choice: LocaleChoice;
  locale: Locale;
  setChoice: (next: LocaleChoice) => void;
} {
  const current = useSyncExternalStore(subscribe, getChoice, getChoice);
  return {
    choice: current,
    locale: resolveLocale(current),
    setChoice: setLocaleChoice,
  };
}

void i18next.use(initReactI18next).init({
  resources: {
    en: { translation: en },
    "zh-Hans": { translation: zhHans },
    "zh-Hant": { translation: zhHant },
  },
  lng: resolveLocale(choice),
  fallbackLng: "en",
  // React escapes on render already; escaping here double-encodes an
  // interpolated session title the moment it contains an apostrophe.
  interpolation: { escapeValue: false },
});

if (typeof document !== "undefined") {
  document.documentElement.lang = resolveLocale(choice);
}

// `system` means "keep following it", so a browser language change moves the
// UI with it — the same contract the `system` theme choice already has.
if (typeof window !== "undefined") {
  window.addEventListener("languagechange", () => {
    if (choice !== "system") return;
    apply(detectLocale());
    listeners.forEach((listener) => listener());
  });
}

export { default as i18n } from "i18next";
