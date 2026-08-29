import { beforeEach, describe, expect, it } from "vitest";
import { detectLocale, resolveLocale, setLocaleChoice } from "./index";

/** `navigator.languages` is read-only, so the detector is exercised by
 * replacing it — the same shape a browser hands over. */
function withLanguages(tags: string[], body: () => void) {
  const original = Object.getOwnPropertyDescriptor(navigator, "languages");
  Object.defineProperty(navigator, "languages", {
    value: tags,
    configurable: true,
  });
  try {
    body();
  } finally {
    if (original) Object.defineProperty(navigator, "languages", original);
    else delete (navigator as { languages?: readonly string[] }).languages;
  }
}

describe("locale detection", () => {
  beforeEach(() => localStorage.clear());

  it("reads Traditional from the script and from the regions that imply it", () => {
    for (const tag of ["zh-Hant", "zh-TW", "zh-HK", "zh-MO"]) {
      withLanguages([tag], () => expect(detectLocale()).toBe("zh-Hant"));
    }
  });

  it("reads every other Chinese as Simplified", () => {
    for (const tag of ["zh", "zh-CN", "zh-SG", "zh-Hans"]) {
      withLanguages([tag], () => expect(detectLocale()).toBe("zh-Hans"));
    }
  });

  it("falls through to the first tag this build can speak", () => {
    // A browser that prefers a language we do not ship still gets the next
    // one it asked for, rather than the English fallback.
    withLanguages(["fr-FR", "zh-TW", "en"], () =>
      expect(detectLocale()).toBe("zh-Hant"),
    );
    withLanguages(["fr-FR", "de"], () => expect(detectLocale()).toBe("en"));
  });

  it("resolves an explicit choice without consulting the browser", () => {
    withLanguages(["zh-TW"], () => {
      expect(resolveLocale("en")).toBe("en");
      expect(resolveLocale("system")).toBe("zh-Hant");
    });
  });

  it("persists the choice and puts it on the document", () => {
    setLocaleChoice("zh-Hant");
    expect(localStorage.getItem("horsie-locale")).toBe("zh-Hant");
    expect(document.documentElement.lang).toBe("zh-Hant");
    setLocaleChoice("en");
    expect(document.documentElement.lang).toBe("en");
  });
});
