#!/usr/bin/env node
/**
 * The AA gate for every world × exposure this app ships.
 *
 * Reads the palettes out of `src/index.css` and `src/skins.css` rather than
 * restating them here, and DISCOVERS the worlds by scanning for `data-skin`
 * blocks rather than carrying a list. A hardcoded list is blind to exactly the
 * case this gate exists for — a world added later and never measured.
 *
 * Exits non-zero on any failure so it can gate a build.
 *
 *   node scripts/contrast.mjs
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(here, "..", "src");

/* ---------- colour ---------- */

function oklchToSrgb(L, C, h) {
  const hr = (h * Math.PI) / 180;
  const a = C * Math.cos(hr);
  const b = C * Math.sin(hr);
  const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
  return [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ].map((x) => Math.min(1, Math.max(0, x)));
}

// WCAG relative luminance wants linear-light values, which is what the matrix
// above already produces — so the sRGB transfer function is applied here and
// nowhere else.
const toLinear = (c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
const luminance = (rgb) => {
  const [r, g, b] = rgb.map((c) => toLinear(gammaEncode(c)));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
};
// The matrix output is linear; encode it to sRGB so clipping happens in the
// same space a browser would clip in, then decode again in `luminance`.
const gammaEncode = (c) =>
  c <= 0.0031308 ? 12.92 * c : 1.055 * c ** (1 / 2.4) - 0.055;

const contrast = (x, y) => {
  const A = luminance(x);
  const B = luminance(y);
  return (Math.max(A, B) + 0.05) / (Math.min(A, B) + 0.05);
};

/** A semi-transparent colour as the browser will actually paint it. A focus
 * ring is declared with an alpha, so measuring the declared colour measures
 * something that never reaches the screen. */
const over = (fg, bg) =>
  fg.alpha === undefined || fg.alpha >= 1
    ? fg
    : fg.map((c, i) => c * fg.alpha + bg[i] * (1 - fg.alpha));

/* ---------- parsing ---------- */

/** Every `selector { --token: oklch(...) }` in a file, as selector → tokens. */
function parseBlocks(css) {
  const blocks = new Map();
  // Strip comments first so a commented-out token never counts as shipped.
  const clean = css.replace(/\/\*[\s\S]*?\*\//g, "");
  const re = /([^{}]+)\{([^{}]*)\}/g;
  let m;
  while ((m = re.exec(clean))) {
    const selector = m[1].trim().replace(/\s+/g, " ");
    const body = m[2];
    const tokens = {};
    const tre =
      /--([a-z-]+)\s*:\s*oklch\(\s*([\d.]+)\s+([\d.]+)\s+([\d.]+)\s*(?:\/\s*([\d.]+)\s*)?\)/g;
    let t;
    while ((t = tre.exec(body))) {
      const rgb = oklchToSrgb(Number(t[2]), Number(t[3]), Number(t[4]));
      // Carried on the array rather than in a parallel map, so every existing
      // consumer keeps treating a token as a plain [r,g,b].
      if (t[5] !== undefined) rgb.alpha = Number(t[5]);
      tokens[t[1]] = rgb;
    }
    if (Object.keys(tokens).length) {
      blocks.set(selector, { ...(blocks.get(selector) ?? {}), ...tokens });
    }
  }
  return blocks;
}

const indexCss = readFileSync(join(src, "index.css"), "utf8");
const skinsCss = readFileSync(join(src, "skins.css"), "utf8");
const blocks = new Map([...parseBlocks(indexCss), ...parseBlocks(skinsCss)]);

const pick = (...selectors) => {
  for (const s of selectors) if (blocks.has(s)) return blocks.get(s);
  return null;
};

/** Paper's light block patches its dark base, so it is resolved by merge;
 * the alternate world's block is complete on its own. */
const dark = pick(':root, [data-theme="dark"]');
const light = { ...dark, ...pick('[data-theme="light"]') };

const PALETTES = [
  ["paper", "dark", dark],
  ["paper", "light", light],
];

const SKIN_IDS = [
  ...new Set([...skinsCss.matchAll(/\[data-skin="([a-z-]+)"\]/g)].map((m) => m[1])),
].sort();
if (!SKIN_IDS.length) {
  console.error("no [data-skin=...] blocks found in skins.css");
  process.exitCode = 1;
}
for (const skin of SKIN_IDS) {
  for (const mode of ["dark", "light"]) {
    const p = pick(`[data-skin="${skin}"][data-theme="${mode}"]`);
    if (!p) {
      console.error(`missing palette: ${skin}/${mode}`);
      process.exitCode = 1;
      continue;
    }
    PALETTES.push([skin, mode, p]);
  }
}

/* ---------- checks ---------- */

const FIELDS = ["chassis", "panel", "panel-raised", "screen"];
const INKS = ["legend", "legend-dim", "legend-faint", "live-ink", "red-ink", "lamp-ok"];
const CODE = ["code-keyword", "code-string", "code-number", "code-type"];
const AA = 4.5;
/** WCAG 1.4.11: a non-text indicator needs 3:1 against what surrounds it. */
const NON_TEXT = 3;
/**
 * Secondary ink, WHILE THE USER IS SELECTING IT.
 *
 * The one floor in this file below AA, and it is a measured trade rather than
 * a rounding-down. A selection wash visible enough to read necessarily moves
 * the ground toward the ink on it: hold every ink at 4.5 and the wash falls to
 * ~1.25 visibility, which is a selection you have to hunt for. The reference
 * implementations sit in the same place — GitHub's dark theme (`#3392FF44`)
 * measures ~1.42 visibility and drops its own muted text to ~4.44.
 *
 * So: primary prose ink holds full AA and lands at 7.3-9.2, well clear.
 * Secondary ink — descriptions, blockquotes — is floored at 4.3 and only
 * while it is actively selected. It is 5.9-6.9 at rest.
 */
const SELECTED_DIM = 4.3;

let failures = 0;
const fail = (msg) => {
  failures++;
  console.log(`  FAIL  ${msg}`);
};

for (const [skin, mode, T] of PALETTES) {
  if (!T) continue;
  console.log(`\n=== ${skin} / ${mode} ===`);

  const missing = [
    ...FIELDS,
    ...INKS,
    ...CODE,
    "keycap",
    "keycap-ink",
    "focus-ring",
    "edge",
    "selection",
    "code-fill",
    "accent",
    "accent-ink",
  ].filter((k) => !T[k]);
  if (missing.length) {
    fail(`palette is missing: ${missing.join(", ")}`);
    continue;
  }

  // Every ink must clear AA on every field it can land on.
  for (const ink of INKS) {
    const worst = Math.min(...FIELDS.map((f) => contrast(T[ink], T[f])));
    if (worst < AA) fail(`${ink} worst ${worst.toFixed(2)} on a field`);
    else console.log(`  ok    ${ink.padEnd(13)} worst ${worst.toFixed(2)}`);
  }

  // Code has its own palette, and it only ever sits on a screen.
  for (const k of CODE) {
    const v = contrast(T[k], T.screen);
    if (v < AA) fail(`${k} ${v.toFixed(2)} on screen`);
  }

  // Keyboard focus has to be *seen*, and the ring is the only indicator on a
  // control that has no other focus state. It is painted with an alpha, so it
  // is measured composited over the surface behind it. Nothing else in this
  // gate covers focus, which is how a light-mode ring at 1.40:1 shipped.
  {
    const worst = Math.min(
      ...FIELDS.map((f) => contrast(over(T["focus-ring"], T[f]), T[f])),
    );
    if (worst < NON_TEXT) fail(`focus-ring worst ${worst.toFixed(2)} on a field`);
    else console.log(`  ok    ${"focus-ring".padEnd(13)} worst ${worst.toFixed(2)}`);
  }

  // The commit and interrupt keys focus in their own ink, drawn *inside* the
  // cap — so the pair that has to separate is ink against the key's own face,
  // not against the panel the key sits on.
  {
    const v = contrast(T["accent-ink"], T.accent);
    if (v < NON_TEXT) fail(`key-go focus ring ${v.toFixed(2)} on the accent face`);
    else console.log(`  ok    ${"key-go focus".padEnd(13)} ${v.toFixed(2)}`);
  }

  // A secondary key is a filled rectangle with no border, so the fill is the
  // only thing separating it from the surface it sits on.
  const capSep = contrast(T.keycap, T.panel);
  if (capSep < 1.25) fail(`keycap:panel separation only ${capSep.toFixed(2)}`);
  const capInk = contrast(T["keycap-ink"], T.keycap);
  if (capInk < AA) fail(`keycap ink ${capInk.toFixed(2)} on the cap`);

  // `panel-raised` is the INTERACTION FILL — hover, selection, a menu above
  // the surface. What it has to do is separate from the ground it lands on,
  // which is lighter in the dark and darker in the light. The old gate held
  // the ordering by brightness in both exposures, which is precisely why the
  // light theme's hover states were invisible: a white fill on a white panel.
  const sep = contrast(T["panel-raised"], T.panel);
  if (sep < 1.2) fail(`interaction fill only ${sep.toFixed(2)} against the panel`);
  else console.log(`  ok    ${"fill:panel".padEnd(13)} ${sep.toFixed(2)}`);

  // Selection is a DIFFERENT job from hover, so it has to be told apart from
  // it — two greys a few points apart cannot say "pointing at" and "picked".
  const sel = contrast(T["accent-quiet"], T["panel-raised"]);
  if (sel < 1.1) fail(`selected fill only ${sel.toFixed(2)} against the hover fill`);
  else console.log(`  ok    ${"select:hover".padEnd(13)} ${sel.toFixed(2)}`);

  // ::selection is a semi-transparent WASH, so the declared colour is never
  // what lands on screen. Every check below is against the composite.
  //
  // Two things have to hold at once and they pull against each other: the
  // wash must be visible against the surface it covers, and the ink on top —
  // which is deliberately NOT overridden, so that code chips and syntax
  // survive being selected — must still clear AA over it.
  for (const under of ["panel", "panel-raised", "screen", "code-fill"]) {
    const painted = over(T.selection, T[under]);
    const seen = contrast(painted, T[under]);
    if (seen < 1.3) fail(`::selection only ${seen.toFixed(2)} on ${under}`);
    const v = contrast(T.legend, painted);
    if (v < AA) fail(`legend on ::selection over ${under} is ${v.toFixed(2)}`);
    // Secondary ink never lands on the inline-code fill — code inherits the
    // prose ink, which is `legend`.
    if (under !== "code-fill") {
      const d = contrast(T["legend-dim"], painted);
      if (d < SELECTED_DIM) {
        fail(`legend-dim on ::selection over ${under} is ${d.toFixed(2)}`);
      }
    }
  }
  console.log(
    `  ok    ${"::selection".padEnd(13)} ${contrast(over(T.selection, T.panel), T.panel).toFixed(2)} seen`,
  );

  // A code span has to be seen with nothing pointing at it, so it is held to
  // a real step off the ground it sits on rather than the interaction fill's
  // whisper — and its ink has to clear AA on it.
  const codeSep = contrast(T["code-fill"], T.panel);
  if (codeSep < 1.35) fail(`inline code fill only ${codeSep.toFixed(2)} against the panel`);
  else console.log(`  ok    ${"code:panel".padEnd(13)} ${codeSep.toFixed(2)}`);
  const codeInk = contrast(T.legend, T["code-fill"]);
  if (codeInk < AA) fail(`legend on the inline code fill ${codeInk.toFixed(2)}`);

  // The chrome frame. A hairline that cannot be seen against the surfaces it
  // divides is a frame nobody drew — which is how the columns came to float.
  for (const under of ["chassis", "panel"]) {
    const v = contrast(T.edge, T[under]);
    if (v < 1.15) fail(`edge only ${v.toFixed(2)} against ${under}`);
    else console.log(`  ok    ${`edge:${under}`.padEnd(13)} ${v.toFixed(2)}`);
  }

  // Machine output is a tint on the surface, and a tint you cannot see is not
  // marking anything.
  const tint = contrast(T.screen, T.panel);
  if (tint < 1.05) fail(`screen tint only ${tint.toFixed(2)} against the panel`);
}

console.log(
  failures === 0
    ? `\nAll ${PALETTES.length} palettes clear AA.`
    : `\n${failures} failure(s).`,
);
process.exit(failures === 0 && !process.exitCode ? 0 : 1);
