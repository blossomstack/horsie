import { describe, expect, it } from "vitest";
import en from "./locales/en";
import zhHans from "./locales/zh-Hans";
import zhHant from "./locales/zh-Hant";

type Catalogue = Record<string, unknown>;

/** Every dotted key with its string value. */
function flatten(node: Catalogue, prefix = ""): Map<string, string> {
  const out = new Map<string, string>();
  for (const [key, value] of Object.entries(node)) {
    const path = prefix ? `${prefix}.${key}` : key;
    if (typeof value === "string") out.set(path, value);
    else if (value && typeof value === "object") {
      for (const [k, v] of flatten(value as Catalogue, path)) out.set(k, v);
    }
  }
  return out;
}

/** `{{name}}` interpolations, sorted. A translation that drops one renders a
 * sentence with a hole in it, which no type can catch. */
const placeholders = (value: string): string[] =>
  [...value.matchAll(/\{\{(\w+)/g)].map((m) => m[1]).sort();

/** `<tag>` slots, sorted. These have to match the `components` prop at the
 * call site, so a renamed one silently drops the element it wrapped. */
const slots = (value: string): string[] =>
  [...value.matchAll(/<(\/?[a-zA-Z][\w]*)>/g)]
    .map((m) => m[1].replace("/", ""))
    .sort();

/** Any CJK ideograph — the cheap test for "somebody actually translated it". */
const hasCjk = (value: string) => /[㐀-鿿]/.test(value);

/** Latin words, once the machine strings are taken out.
 *
 * A URL, an image reference or a path is the same in every language — it is
 * something to be pasted, not read — so counting its segments as English
 * words would flag every correctly-untouched placeholder in the catalogue. */
const latinWords = (value: string) =>
  value
    .replace(/\{\{\w+\}\}/g, " ")
    .replace(/<\/?[\w]+>/g, " ")
    .replace(/\S*[/:@]\S*/g, " ")
    .match(/[A-Za-z]{2,}/g) ?? [];

/** Every HTML element name. A slot named after one is parsed as that element
 * rather than mapped to the `components` prop — and if it is a void element
 * (`<link>`), its children are dropped from the sentence entirely. */
const HTML_TAGS = new Set(
  ("a abbr address area article aside audio b base bdi bdo blockquote body br " +
    "button canvas caption cite code col colgroup data datalist dd del details " +
    "dfn dialog div dl dt em embed fieldset figcaption figure footer form h1 h2 " +
    "h3 h4 h5 h6 head header hgroup hr html i iframe img input ins kbd label " +
    "legend li link main map mark menu meta meter nav noscript object ol optgroup " +
    "option output p param picture pre progress q rp rt ruby s samp script " +
    "search section select slot small source span strong style sub summary sup " +
    "table tbody td template textarea tfoot th thead time title tr track u ul " +
    "var video wbr").split(" "),
);

const LOCALES = [
  ["zh-Hans", zhHans],
  ["zh-Hant", zhHant],
] as const;

const source = flatten(en as Catalogue);

describe("catalogues", () => {
  // A scan over the whole source catalogue, not a list of keys somebody
  // remembered to add: a key added to `en` is covered here the moment it
  // exists.
  it("has a non-trivial number of keys", () => {
    expect(source.size).toBeGreaterThan(500);
  });

  it("names no markup slot after an HTML element", () => {
    const collisions: string[] = [];
    for (const [key, value] of source) {
      for (const slot of slots(value)) {
        if (HTML_TAGS.has(slot)) collisions.push(`${key}: <${slot}>`);
      }
    }
    expect(collisions).toEqual([]);
  });

  for (const [name, catalogue] of LOCALES) {
    describe(name, () => {
      const translated = flatten(catalogue as Catalogue);

      it("covers exactly the source key set", () => {
        expect([...translated.keys()].sort()).toEqual(
          [...source.keys()].sort(),
        );
      });

      it("keeps every interpolation the source declares", () => {
        const broken: string[] = [];
        for (const [key, value] of translated) {
          const want = placeholders(source.get(key)!);
          if (String(placeholders(value)) !== String(want)) broken.push(key);
        }
        expect(broken).toEqual([]);
      });

      it("keeps every markup slot the source declares", () => {
        const broken: string[] = [];
        for (const [key, value] of translated) {
          const want = slots(source.get(key)!);
          if (String(slots(value)) !== String(want)) broken.push(key);
        }
        expect(broken).toEqual([]);
      });

      // A stub catalogue type-checks and renders English at people who asked
      // for Chinese, so the shape alone proves nothing. A sentence — three or
      // more Latin words — with no ideograph in it was not translated.
      it("has no untranslated sentences", () => {
        const untranslated: string[] = [];
        for (const [key, value] of translated) {
          if (latinWords(value).length >= 3 && !hasCjk(value)) {
            untranslated.push(`${key}: ${value}`);
          }
        }
        expect(untranslated).toEqual([]);
      });
    });
  }
});
