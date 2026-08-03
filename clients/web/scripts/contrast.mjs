// Scratch: OKLCH -> sRGB -> WCAG contrast, to derive both ramps numerically.
function srgb(L, C, h) {
  const hr = (h * Math.PI) / 180;
  const a = C * Math.cos(hr), b = C * Math.sin(hr);
  const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
  const lin = [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ];
  return lin.map((x) => Math.min(1, Math.max(0, x)));
}
const lum = ([r, g, b]) => 0.2126 * r + 0.7152 * g + 0.0722 * b;
const cr = (x, y) => {
  const A = lum(x), B = lum(y);
  return (Math.max(A, B) + 0.05) / (Math.min(A, B) + 0.05);
};
const c = (L, C, h) => srgb(L, C, h);

const DARK = {
  chassis: c(0.19, 0.005, 255), panel: c(0.235, 0.006, 255),
  raised: c(0.285, 0.007, 255), screen: c(0.155, 0.006, 255),
  keycap: c(0.82, 0.016, 90), keycapInk: c(0.22, 0.008, 255),
  legend: c(0.93, 0.008, 90), dim: c(0.71, 0.012, 90), faint: c(0.655, 0.013, 90),
  orange: c(0.688, 0.196, 42), orangeInk: c(0.19, 0.02, 42),
  amberInk: c(0.8, 0.155, 78), redInk: c(0.7, 0.2, 27), lampOk: c(0.78, 0.16, 158),
  kw: c(0.78, 0.15, 40), str: c(0.8, 0.13, 155), num: c(0.82, 0.13, 78), typ: c(0.8, 0.1, 220),
};
const LIGHT = {
  chassis: c(0.915, 0.009, 88), panel: c(0.955, 0.006, 88),
  raised: c(0.978, 0.004, 88), screen: c(0.878, 0.012, 88),
  keycap: c(0.44, 0.014, 255), keycapInk: c(0.965, 0.006, 88),
  legend: c(0.245, 0.008, 255), dim: c(0.395, 0.011, 255), faint: c(0.475, 0.013, 255),
  orange: c(0.545, 0.2, 42), orangeInk: c(0.99, 0.01, 42),
  amberInk: c(0.47, 0.12, 68), redInk: c(0.5, 0.21, 27), lampOk: c(0.44, 0.115, 158),
  kw: c(0.47, 0.17, 40), str: c(0.44, 0.12, 155), num: c(0.47, 0.12, 65), typ: c(0.46, 0.1, 235),
};

const FIELDS = ["chassis", "panel", "raised", "screen"];
const INKS = ["legend", "dim", "faint", "amberInk", "redInk", "lampOk"];
for (const [name, T] of [["DARK", DARK], ["LIGHT", LIGHT]]) {
  console.log(`\n=== ${name} ===`);
  console.log("material separation:");
  console.log(
    "  keycap:panel", cr(T.keycap, T.panel).toFixed(2),
    "| keycap:chassis", cr(T.keycap, T.chassis).toFixed(2),
    "| screen:panel", cr(T.screen, T.panel).toFixed(2),
    "| raised:panel", cr(T.raised, T.panel).toFixed(2),
    "| panel:chassis", cr(T.panel, T.chassis).toFixed(2),
  );
  console.log("  keycap ink on cap", cr(T.keycapInk, T.keycap).toFixed(2),
    "| orange ink on orange", cr(T.orangeInk, T.orange).toFixed(2));
  console.log("text on fields (need >= 4.5):");
  for (const ink of INKS) {
    const row = FIELDS.map((f) => `${f} ${cr(T[ink], T[f]).toFixed(2)}`).join("  ");
    const worst = Math.min(...FIELDS.map((f) => cr(T[ink], T[f])));
    console.log(`  ${ink.padEnd(9)} ${row}   ${worst >= 4.5 ? "OK" : "FAIL " + worst.toFixed(2)}`);
  }
  console.log("code tokens on screen (need >= 4.5):");
  for (const k of ["kw", "str", "num", "typ"]) {
    const v = cr(T[k], T.screen);
    console.log(`  ${k.padEnd(4)} ${v.toFixed(2)} ${v >= 4.5 ? "OK" : "FAIL"}`);
  }
}
