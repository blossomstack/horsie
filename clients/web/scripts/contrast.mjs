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

/** Graphite's light block patches its dark base, so it is resolved by merge;
 * every alternate world's block is complete on its own. */
const dark = pick(':root, [data-theme="dark"]');
const light = { ...dark, ...pick('[data-theme="light"]') };

const PALETTES = [
  ["graphite", "dark", dark],
  ["graphite", "light", light],
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
  if (sep < 1.1) fail(`interaction fill only ${sep.toFixed(2)} against the panel`);
  else console.log(`  ok    ${"fill:panel".padEnd(13)} ${sep.toFixed(2)}`);

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
