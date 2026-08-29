/**
 * Finds English still hard-coded in the UI, and `t()` keys no catalogue has.
 *
 * A scan, not a list: it walks every source file under `src/`, so a page added
 * tomorrow is covered without anyone remembering to register it. A
 * hand-maintained list of "files that are translated" would be blind to
 * exactly the case it exists for.
 *
 * Hand-rolled rather than AST-based because the installed TypeScript exposes
 * no stable parser API. It is a lint, so it is allowed to be approximate in
 * the safe direction: `// i18n-ignore` on the line above silences a match that
 * is genuinely not prose.
 *
 * Run: `bun run i18n:audit`. The unit suite asserts it stays clean.
 */
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";

// Under vitest this module is served by vite, so `import.meta.url` carries a
// `/@fs` prefix that is not part of the real path.
const ROOT = new URL("..", import.meta.url).pathname.replace(/^\/@fs/, "");
const SRC = join(ROOT, "src");

/** Attributes whose string value is read by a person, not a machine. */
const HUMAN_ATTRS =
  "title|aria-label|aria-description|aria-valuetext|placeholder|alt|label|desc|description|confirmLabel|emptyText|what|hint|addLabel|closeLabel|saveLabel|deleteLabel|legend|subtitle|blurb|message|summary";

const SKIP = (p) =>
  p.includes("/generated/") ||
  p.includes("/i18n/locales/") ||
  /\.test\.tsx?$/.test(p) ||
  p.endsWith("vitest.setup.ts");

function walk(dir, out = []) {
  for (const name of readdirSync(dir).sort()) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) walk(p, out);
    else if (/\.tsx?$/.test(p) && !SKIP(p)) out.push(p);
  }
  return out;
}

/** Blanks out comments and template literals, preserving offsets so a match's
 * line number still points at the real line. Comments in this codebase are
 * long and full of prose and angle brackets, and every one of them would
 * otherwise read as an untranslated string.
 *
 * Template literals are blanked for the JSX rules and scanned separately by
 * `templateStrings` — a sentence assembled with `${…}` in it is exactly the
 * kind of string that gets missed, and blanking it here without the second
 * pass is how "Add provider" shipped untranslated. */
function blankNonCode(src) {
  const out = src.split("");
  let i = 0;
  const blank = (from, to) => {
    for (let k = from; k < to; k++) if (out[k] !== "\n") out[k] = " ";
  };
  while (i < src.length) {
    const two = src.slice(i, i + 2);
    if (two === "//") {
      const end = src.indexOf("\n", i);
      blank(i, end === -1 ? src.length : end);
      i = end === -1 ? src.length : end;
    } else if (two === "/*") {
      const end = src.indexOf("*/", i + 2);
      const stop = end === -1 ? src.length : end + 2;
      blank(i, stop);
      i = stop;
    } else if (src[i] === "`") {
      let k = i + 1;
      while (k < src.length && !(src[k] === "`" && src[k - 1] !== "\\")) k++;
      blank(i, Math.min(k + 1, src.length));
      i = k + 1;
    } else {
      i++;
    }
  }
  const blanked = out.join("");
  // A `<Trans>` body is the catalogue entry's own fallback markup, not a
  // string anyone forgot to translate.
  return blanked.replace(
    /<Trans\b[\s\S]*?<\/Trans>/g,
    (m) => m.replace(/[^\n]/g, " "),
  );
}

/** Comments only, leaving template literals intact for the second pass. */
function blankComments(src) {
  const out = src.split("");
  let i = 0;
  const blank = (from, to) => {
    for (let k = from; k < to; k++) if (out[k] !== "\n") out[k] = " ";
  };
  while (i < src.length) {
    const two = src.slice(i, i + 2);
    if (two === "//") {
      const end = src.indexOf("\n", i);
      blank(i, end === -1 ? src.length : end);
      i = end === -1 ? src.length : end;
    } else if (two === "/*") {
      const end = src.indexOf("*/", i + 2);
      const stop = end === -1 ? src.length : end + 2;
      blank(i, stop);
      i = stop;
    } else {
      i++;
    }
  }
  return out.join("");
}

/** Every template literal, with its `${…}` holes punched out. */
function templateStrings(src) {
  const out = [];
  let i = 0;
  while (i < src.length) {
    if (src[i] !== "`") {
      // Skip a plain string so a backtick inside one does not open a literal.
      if (src[i] === '"' || src[i] === "'") {
        const quote = src[i++];
        while (i < src.length && !(src[i] === quote && src[i - 1] !== "\\")) i++;
      }
      i++;
      continue;
    }
    const start = i++;
    let depth = 0;
    let text = "";
    while (i < src.length) {
      if (depth === 0 && src[i] === "`" && src[i - 1] !== "\\") break;
      if (depth === 0 && src[i] === "$" && src[i + 1] === "{") {
        depth = 1;
        i += 2;
        text += " ";
        continue;
      }
      if (depth > 0) {
        if (src[i] === "{") depth++;
        else if (src[i] === "}") depth--;
        i++;
        continue;
      }
      text += src[i++];
    }
    out.push({ index: start, text });
    i++;
  }
  return out;
}

/** Words, as opposed to punctuation, an ellipsis, or a lone symbol. */
const isProse = (text) => /[A-Za-z]{2,}/.test(text);

/** A generic argument list, a call, or an assignment that the `>text<` shape
 * matched by accident — no sentence in this UI contains any of these. */
const looksLikeCode = (text) => /[(){}[\]=;:"'|&$@\\]|=>/.test(text);

/** Every dotted key in the source catalogue, plural suffixes folded away.
 * The catalogue is prettier-formatted with a fixed two-space indent, so its
 * nesting is readable from the indentation alone. */
export function catalogueKeys() {
  const src = readFileSync(join(SRC, "i18n/locales/en.ts"), "utf8");
  const keys = new Set();
  const stack = [];
  let pending = null;
  for (const raw of src.split("\n")) {
    const line = raw.trim();
    if (line.startsWith("//") || line.startsWith("*") || line.startsWith("/*"))
      continue;
    if (line === "};" || line === "}," || line === "}") {
      stack.pop();
      continue;
    }
    const open = line.match(/^([A-Za-z0-9_]+):\s*\{$/);
    if (open) {
      stack.push(open[1]);
      continue;
    }
    const leaf = line.match(/^([A-Za-z0-9_]+):\s*(.*)$/);
    if (leaf) {
      const name = leaf[1];
      // Prettier wraps a long value onto its own line, leaving `key:` alone.
      const path = [...stack, name].join(".");
      keys.add(path.replace(/_(zero|one|two|few|many|other)$/, ""));
      pending = leaf[2] === "" ? path : null;
      continue;
    }
    if (pending) pending = null;
  }
  return keys;
}

export function audit() {
  const known = catalogueKeys();
  const findings = [];
  for (const path of walk(SRC)) {
    const rel = relative(ROOT, path);
    const raw = readFileSync(path, "utf8");
    const src = blankNonCode(raw);
    const lineAt = (index) => raw.slice(0, index).split("\n").length;
    const ignored = new Set(
      raw
        .split("\n")
        .flatMap((l, n) => (l.includes("i18n-ignore") ? [n + 2, n + 1] : [])),
    );
    const push = (index, kind, text) => {
      const line = lineAt(index);
      if (!ignored.has(line)) findings.push({ rel, line, kind, text });
    };

    // JSX text: between a closing `>` and an opening `<`, with no brace or
    // angle bracket in between — which is exactly the shape of a text node.
    // Only in `.tsx`: in a plain `.ts` the same shape is a nested generic,
    // and `Promise<Session>` is not a string anybody reads.
    if (path.endsWith(".tsx")) {
      for (const m of src.matchAll(/>[ \t\r\n]*([^<>{}]*?)[ \t\r\n]*</g)) {
        const text = m[1].replace(/\s+/g, " ").trim();
        if (isProse(text) && !looksLikeCode(text)) {
          push(m.index, "jsx-text", text);
        }
      }
    }

    // A human-facing attribute given a literal, either bare or braced.
    const attr = new RegExp(
      `\\b(${HUMAN_ATTRS})\\s*=\\s*\\{?\\s*"([^"]*)"\\s*\\}?`,
      "g",
    );
    for (const m of src.matchAll(attr)) {
      if (isProse(m[2])) push(m.index, `attr:${m[1]}`, m[2]);
    }

    // A sentence assembled in a template literal. Two words is the bar: one
    // word is a class name, a slug or an id far more often than it is prose.
    for (const { index, text } of templateStrings(blankComments(raw))) {
      const words = text.match(/[A-Za-z]{2,}/g) ?? [];
      const prose = text.trim();
      if (
        words.length >= 2 &&
        / [a-z]/.test(prose) &&
        // Quotes are ordinary punctuation in a sentence, so the JSX-text
        // code test is too strict here; only real expression syntax counts.
        !/[(){}[\];=|&$@\\]|=>|^https?:/.test(prose) &&
        !/^[a-z-]+(\s+[a-z-]+)*$/.test(prose)
      ) {
        push(index, "template-string", prose);
      }
    }

    // `t("…")` against a key nothing defines renders the key itself on screen.
    for (const m of src.matchAll(/\bt\(\s*"([^"]+)"/g)) {
      if (!known.has(m[1])) push(m.index, "missing-key", m[1]);
    }
  }
  return findings;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const findings = audit();
  for (const f of findings) {
    console.log(`${f.rel}:${f.line}  [${f.kind}]  ${f.text.slice(0, 90)}`);
  }
  console.log(`\n${findings.length} finding(s)`);
  process.exit(findings.length ? 1 : 0);
}
