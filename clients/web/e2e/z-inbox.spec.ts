// Group Z — the inbox: what agents address to the person steering them.
//
// The two kinds behave oppositely and both loops are here. `notify_user` does
// not stop the agent, so its row is news; `ask_user` does, so its row is an
// agent that has halted and the row is the only place outside the session where
// that is visible.
//
// The answered-in-the-session-page case is the one worth spelling out. Both
// pages answer through the same `POST /sessions/:id/answers`, and the inbox row
// is settled by two writers that race — the session actor closing it when the
// agent resumes, and the answer handler recording what the answer was. If the
// close won, a question the person answered would read as merely closed. Only a
// real server can order those two, which is why it is asserted here rather than
// in a unit test.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus, projectRoot } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

/** The rail's inbox link, once its badge reports `count` open asks. */
async function openInbox(page: import("@playwright/test").Page) {
  await page.getByTestId("inbox-link").click();
  await expect(page.getByTestId("inbox-page")).toBeVisible();
}

test("Z1: a question reaches the inbox and is answered there", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("ask_user", {
    question: "Which database should this point at?",
    choices: ["postgres", "sqlite"],
  });
  await mock.queueText("Postgres it is.");
  await createSession(page, appBase);
  await sendMessage(page, "migrate the journal");
  await expectStatus(page, "AwaitingInput");

  // The badge is the whole point: the question is visible without opening the
  // session it stopped.
  await expect(page.getByTestId("inbox-badge")).toBeVisible();
  await openInbox(page);

  const row = page.getByTestId("inbox-row").first();
  await expect(row).toContainText("Which database should this point at?");
  await row.getByTestId("inbox-open").click();

  const message = page.getByTestId("inbox-message");
  await expect(message).toContainText("Which database should this point at?");
  // The same answer control the transcript offers, from the same component —
  // a question answerable in one place and not the other is the bug this
  // shares a component to avoid.
  await message.getByTestId("ask-user-choice").filter({ hasText: "postgres" }).click();
  await message.getByTestId("ask-user-send").click();

  // Answering resumes the agent, and the row records what became of it.
  // Not asserted by watching the badge go out: the suite shares one server, so
  // by this point other specs' messages are in the same inbox and the badge is
  // counting them too. What the badge does with a count is a Sidebar unit test.
  await expect(message.getByTestId("inbox-outcome")).toBeVisible();

  // And the answer really reached the model, rather than merely settling a row.
  await expect
    .poll(async () => (await mock.received()).length, { timeout: 15_000 })
    .toBeGreaterThan(1);
});

test("Z2: a question answered in the session page reads as answered in the inbox", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("ask_user", { question: "Which branch?" });
  await mock.queueText("Done on main.");
  await createSession(page, appBase);
  await sendMessage(page, "cut a release");
  await expectStatus(page, "AwaitingInput");

  // Answered where it was asked, never touching the inbox.
  const card = page.getByTestId("ask-user-card");
  await card.getByTestId("ask-user-text").fill("main");
  await card.getByTestId("ask-user-send").click();
  await expectStatus(page, "Idle");

  await openInbox(page);
  await page.getByTestId("inbox-row").first().getByTestId("inbox-open").click();
  const outcome = page.getByTestId("inbox-message").getByTestId("inbox-outcome");
  await expect(outcome).toBeVisible();
  // "Answered", not "closed". The distinction is the race this test exists for:
  // the projection closes the row the moment the agent resumes, and the answer
  // handler has to be able to land its outcome either side of that.
  await expect(outcome).not.toContainText(/closed/i);
  await expect(outcome).toContainText(/answered/i);
});

test("Z3: a notice arrives without stopping the agent, and can be deleted", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("notify_user", {
    title: "Fixtures look wrong",
    body: "Two of them predate the rename.",
  });
  await mock.queueText("Carried on regardless.");
  await createSession(page, appBase);
  await sendMessage(page, "check the fixtures");

  // The agent kept working: the turn ran to completion rather than parking.
  await expectStatus(page, "Idle");

  await openInbox(page);
  const row = page.getByTestId("inbox-row").first();
  await expect(row).toContainText("Fixtures look wrong");
  await row.getByTestId("inbox-open").click();
  await expect(page.getByTestId("inbox-message")).toContainText(
    "Two of them predate the rename.",
  );
  // A notice links back to the agent that left it.
  await expect(page.getByTestId("inbox-open-session")).toHaveAttribute(
    "href",
    new RegExp(`${projectRoot()}/sessions/`),
  );

  const id = await row.getAttribute("data-message-id");
  await page.getByTestId(`inbox-select-${id}`).click();
  await page.getByTestId("inbox-delete-selected").click();
  await page.getByTestId("confirm-accept").click();
  // This row, not an empty inbox: the suite shares one server, so earlier
  // specs' messages are still here and asserting on emptiness would be
  // asserting about them.
  await expect(page.locator(`[data-message-id="${id}"]`)).toHaveCount(0);
});

test("Z4: deleting an open question warns that it declines it, and the agent carries on", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("ask_user", { question: "Which database?" });
  await mock.queueText("Assumed postgres.");
  await createSession(page, appBase);
  await sendMessage(page, "migrate the journal");
  await expectStatus(page, "AwaitingInput");

  await openInbox(page);
  const row = page.getByTestId("inbox-row").first();
  const id = await row.getAttribute("data-message-id");
  await page.getByTestId(`inbox-select-${id}`).click();
  await page.getByTestId("inbox-delete-selected").click();

  // The warning is the whole reason this is not a plain delete: the row is an
  // agent that has stopped, and dropping it silently would strand it.
  const dialog = page.getByTestId("confirm-dialog");
  await expect(dialog).toContainText(/declin/i);
  await dialog.getByTestId("confirm-accept").click();

  await expect(page.locator(`[data-message-id="${id}"]`)).toHaveCount(0);
  // Declined through the ordinary answer path, so the agent was told and
  // resumed rather than being left parked with its row deleted.
  await expect
    .poll(async () => (await mock.received()).length, { timeout: 15_000 })
    .toBeGreaterThan(1);
});
