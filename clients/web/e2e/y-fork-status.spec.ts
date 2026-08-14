import { expect, test } from "./fixtures";
import { createSession, expectStatus, sendMessage } from "./helpers";

/** A fork's status, as a reader sees it.
 *
 * The reported bug was entirely a reading: the fork answered, spent nothing
 * more, and went on saying `RUNNING` — through reloads and through a server
 * restart, because a page's status is folded from that agent's own log and the
 * turn's end was never written there. So the assertion that matters is the
 * badge on the fork's own page after it has visibly answered, and the reload is
 * part of the test rather than a flourish: a live subscription could paper over
 * a log that never got the boundary. */
test("Y1: a fork that has answered reads Idle, and still does after a reload", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.reset();
  await mock.queueText("the original answer");
  await mock.queueText("the fork's answer");

  await createSession(page, appBase);
  await sendMessage(page, "start the migration");
  await expect(page.getByTestId("transcript-scroll")).toContainText(
    "the original answer",
  );
  await expectStatus(page, "Idle");

  // `/fork` redirects to the new conversation, so what follows is read on the
  // fork's own page and not on the one it branched from.
  await sendMessage(page, "/fork try the other way");
  await page.waitForURL(/\/agents\/[0-9a-f-]+$/);
  await expect(page.getByTestId("transcript-scroll")).toContainText(
    "the fork's answer",
  );

  await expectStatus(page, "Idle");
  await page.reload();
  await expect(page.getByTestId("transcript-scroll")).toContainText(
    "the fork's answer",
  );
  await expectStatus(page, "Idle");
});

/** The session the fork came from is not the thing that was working.
 *
 * The two statuses are read off different things — the fork's log and the
 * session's own state — and a client shows them side by side, so a fork's turn
 * must move exactly one of them.
 *
 * Note what this is and is not: it passed before Y1's bug was fixed, because a
 * fork moved *neither* status then. It is here to hold the fix down rather than
 * to have caught the fault — closing a fork's turn through the session's own
 * `TurnEnded` would have fixed Y1 and broken this. */
test("Y2: a fork's turn leaves the conversation it branched from idle", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.reset();
  await mock.queueText("the original answer");
  await mock.queueText("the fork's answer");

  await createSession(page, appBase);
  const session = await sendMessage(page, "start the migration");
  await expectStatus(page, "Idle");

  await sendMessage(page, "/fork try the other way");
  await page.waitForURL(/\/agents\/[0-9a-f-]+$/);
  await expect(page.getByTestId("transcript-scroll")).toContainText(
    "the fork's answer",
  );

  await page.goto(`${appBase}/sessions/${session}`);
  await expect(page.getByTestId("transcript-scroll")).toContainText(
    "the original answer",
  );
  await expectStatus(page, "Idle");
});

/** Stop, pressed on a fork's page.
 *
 * There was no way to do this at all: the stop call named no agent, so it could
 * only ever mean the main agent. On a fork's page the button cancelled a turn
 * the reader was not looking at — and once the fork was the thing running, the
 * gate read the session's status, found it idle, and returned success having
 * done nothing. */
test("Y3: stopping a fork stops the fork, and leaves the conversation it came from alone", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.reset();
  await mock.queueText("the original answer");
  // Long enough that the turn cannot end on its own inside the test. A short
  // sleep here made this pass with the stop wired to the wrong agent: the
  // assertion below was satisfied by the turn finishing, not by it being
  // stopped.
  await mock.queueToolCall("bash", { command: "sleep 30" });

  await createSession(page, appBase);
  const session = await sendMessage(page, "start the migration");
  await expectStatus(page, "Idle");

  await sendMessage(page, "/fork try the other way");
  await page.waitForURL(/\/agents\/[0-9a-f-]+$/);
  await expectStatus(page, "Running");

  await page.getByTestId("composer-stop").click();
  await expectStatus(page, "Idle");

  // And the conversation it branched from was never touched.
  await page.goto(`${appBase}/sessions/${session}`);
  await expect(page.getByTestId("transcript-scroll")).toContainText(
    "the original answer",
  );
  await expectStatus(page, "Idle");
});
