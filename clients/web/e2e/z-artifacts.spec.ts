// Group Z — image attachments, end to end through the real stack.
//
// Real horsie-server + mock LLM + real local runtime vendor. The point of
// doing this at the browser level rather than in vitest is that everything
// below the paste is real: the bytes go up to the artifact route, the server
// sniffs them, a row is written, and the `<img>` in the transcript fetches
// them back by content hash over an authenticated request. None of that is
// exercised by a component test with a mocked `fetch`.

import { test, expect, type Page } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

/** A real 1x1 PNG. Small enough to inline, real enough to sniff. */
const PNG_BASE64 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/**
 * Paste a file into the composer.
 *
 * Built inside the page rather than with `setInputFiles`, because the paste
 * path and the file-input path are different code and pasting is the one this
 * feature exists for. `DataTransfer` is constructed in the browser context so
 * the `ClipboardEvent` carries a genuine `File`.
 */
async function pasteFile(
  page: Page,
  base64: string,
  name: string,
  type: string,
): Promise<void> {
  await page.evaluate(
    async ({ base64, name, type }) => {
      const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
      const file = new File([bytes], name, { type });
      const dt = new DataTransfer();
      dt.items.add(file);
      const target = document.querySelector<HTMLElement>(
        '[data-testid="composer-input"]',
      );
      if (!target) throw new Error("no composer input to paste into");
      target.dispatchEvent(
        new ClipboardEvent("paste", {
          clipboardData: dt,
          bubbles: true,
          cancelable: true,
        }),
      );
    },
    { base64, name, type },
  );
}

test("Z1: a pasted image uploads, sends, and renders from the server", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("I can see it.");
  await createSession(page, appBase);

  await pasteFile(page, PNG_BASE64, "shot.png", "image/png");

  // The attachment settles before the send is allowed — the upload happens on
  // attach, not on send, so a thumbnail with no id behind it must never be
  // sendable.
  const attachment = page.getByTestId("composer-attachment");
  await expect(attachment).toBeVisible();
  await expect(page.getByTestId("composer-attachment-error")).toHaveCount(0);

  await sendMessage(page, "what is this?");
  await expectStatus(page, "Idle");

  // The tray empties once the message is away, so a second turn does not
  // re-send the same image.
  await expect(page.getByTestId("composer-attachment")).toHaveCount(0);

  // The transcript renders it, and the `<img>` actually resolves — a broken
  // src would still be "visible" to Playwright, so assert the decoded size.
  // The testid sits on the button that opens the lightbox; the `<img>` is
  // inside it. Reading `complete` off the button silently yields `undefined`,
  // which is an assertion that can never pass and never says why.
  const thumb = page.getByTestId("artifact-image").first();
  await expect(thumb).toBeVisible();
  const image = thumb.locator("img");
  // Poll rather than read once: `toBeVisible` resolves as soon as the element
  // is laid out, which is well before the browser has fetched the bytes. The
  // distinction that matters is `complete && naturalWidth > 0` — an element
  // that finished loading a 404 is `complete` with a zero natural width, so
  // this separates "still fetching" from "broken src".
  await expect
    .poll(
      () =>
        image.evaluate(
          (el: HTMLImageElement) => (el.complete ? el.naturalWidth : null),
        ),
      { message: "the image never decoded — a broken src stays at zero width" },
    )
    .toBeGreaterThan(0);

  // The URL is the artifact route, keyed by content hash.
  const src = await image.getAttribute("src");
  expect(src).toContain("/artifacts/");
});

test("Z2: an unsupported file is refused and never becomes an attachment", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("ok");
  await createSession(page, appBase);

  // A plain-text file renamed to look like an image. The server sniffs the
  // bytes rather than trusting the declared type, so this is refused even
  // though the MIME type claims PNG.
  const notAnImage = btoa("this is not a png, whatever the type says");
  await pasteFile(page, notAnImage, "trap.png", "image/png");

  // Either the client refuses it outright or the server does; both must end
  // with no sendable attachment and a visible reason.
  await expect(page.getByTestId("composer-attachment-error").or(page.getByTestId("composer-attach-notice"))).toBeVisible();
  await expect(page.getByTestId("artifact-image")).toHaveCount(0);
});

test("Z3: clicking an image opens the lightbox, Escape closes it", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("seen");
  await createSession(page, appBase);
  await pasteFile(page, PNG_BASE64, "shot.png", "image/png");
  await expect(page.getByTestId("composer-attachment")).toBeVisible();
  await sendMessage(page, "look");
  await expectStatus(page, "Idle");

  await page.getByTestId("artifact-image").first().click();

  const lightbox = page.getByTestId("lightbox");
  await expect(lightbox).toBeVisible();
  await expect(page.getByTestId("lightbox-download")).toBeVisible();

  // The backdrop must cover the whole viewport. It is portalled to <body> for
  // exactly this reason: a transcript turn runs a filling transform animation,
  // which makes it the containing block for `position: fixed` and would clip
  // the backdrop to one message.
  const box = await page.getByTestId("lightbox-backdrop").boundingBox();
  const viewport = page.viewportSize();
  expect(box, "the backdrop should be laid out").not.toBeNull();
  if (box && viewport) {
    expect(box.height).toBeGreaterThanOrEqual(viewport.height - 1);
  }

  await page.keyboard.press("Escape");
  await expect(lightbox).toHaveCount(0);
});
