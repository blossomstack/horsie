import { chromium } from "@playwright/test";

const svg = (label, color) =>
  `<svg xmlns="http://www.w3.org/2000/svg" width="900" height="620" viewBox="0 0 200 140"><rect width="200" height="140" fill="${color}"/><text x="100" y="76" font-family="sans-serif" font-size="18" fill="#fff" text-anchor="middle">${label}</text></svg>`;

const colors = { a: ["photo A", "#3b6ea5"], b: ["photo B", "#8a5b9c"], c: ["shot C", "#4a8a63"] };

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1100, height: 1200 }, deviceScaleFactor: 2 });
// One upload succeeds slowly, one fails: the tray then shows ready,
// uploading and error at once.
let uploads = 0;
await page.route("**/api/p/demo/artifacts?**", async (route) => {
  const n = uploads++;
  if (n === 1) { await new Promise((r) => setTimeout(r, 20_000)); }
  if (n === 2) return route.fulfill({ status: 413, contentType: "application/json", body: JSON.stringify({ code: "too_large", message: "That file is larger than this server accepts." }) });
  route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ id: "a", mediaType: "image/png", byteSize: 1000, kind: { kind: "Image", value: { width: 10, height: 10 } }, filename: "diagram.png" }) });
});
await page.route("**/api/p/demo/artifacts/*", (route) => {
  const id = route.request().url().split("/").pop();
  const [label, color] = colors[id] ?? ["file", "#888"];
  route.fulfill({ status: 200, contentType: "image/svg+xml", body: svg(label, color) });
});
await page.goto("http://localhost:5391/harness.html", { waitUntil: "networkidle" });
await page.getByTestId("tool-call-toggle").click();
await page.waitForTimeout(400);
await page.screenshot({ path: "/tmp/shots/01-transcript.png", fullPage: true });

// The composer with a tray: three attachments in three states.
await page.evaluate(async () => {
  // Real PNG bytes, so the tray's blob-URL preview actually decodes.
  const paint = (color) =>
    new Promise((resolve) => {
      const c = document.createElement("canvas");
      c.width = 120; c.height = 90;
      const ctx = c.getContext("2d");
      ctx.fillStyle = color; ctx.fillRect(0, 0, 120, 90);
      c.toBlob(resolve, "image/png");
    });
  const dt = new DataTransfer();
  const bytes = new Uint8Array([137, 80, 78, 71]);
  dt.items.add(new File([await paint("#3b6ea5")], "diagram.png", { type: "image/png" }));
  dt.items.add(new File([await paint("#8a5b9c")], "still-going-up.png", { type: "image/png" }));
  dt.items.add(new File([bytes], "protocol-spec.pdf", { type: "application/pdf" }));
  const input = document.querySelector('[data-testid="composer-file-input"]');
  input.files = dt.files;
  input.dispatchEvent(new Event("change", { bubbles: true }));
});
await page.getByTestId("composer-input").fill("Two files attached, one still going up.");
await page.waitForTimeout(1500);
await page.locator("#composer-slot").screenshot({ path: "/tmp/shots/02-composer-tray.png" });

// The lightbox.
await page.getByTestId("artifact-image").first().click();
await page.waitForTimeout(300);
await page.evaluate(() => window.scrollTo(0, 0));
await page.waitForTimeout(200);
await page.screenshot({ path: "/tmp/shots/03-lightbox.png", fullPage: false });
await browser.close();
console.log("done");
