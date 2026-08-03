// Throwaway screenshot driver. Sorts last so it never contaminates the FIFO
// mock queue. DELETE BEFORE COMMITTING.
import { test } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

const SKINS = ["console", "paper", "soft", "slate"] as const;
const MODES = ["dark", "light"] as const;

test("shots", async ({ page, appBase, mock }) => {
  await mock.reset();
  await mock.queueToolCall("task_list", {
    action: "create",
    tasks: ["Read the spec", "Rework the prose styles", "Measure contrast"],
  });
  await mock.queueText(
    "Here is what I found.\n\n" +
      "## Design (Approach A)\n\n" +
      "**Command surface** (each takes `--server`, resolving like every other server command):\n\n" +
      "```\nhorsie routines list      -> GET /api/routines\nhorsie routines invoke <name>  -> POST /api/routines/:name/run\n```\n\n" +
      "`cli/src/server_client.rs` — two additions, one JSON round-trip each via the existing `send` helper:\n\n" +
      "- `list_routines() -> Vec<RoutineView>` (GET `/api/routines`)\n" +
      "- `run_routine(name) -> RoutineRunResponse` (POST `/api/routines/{name}/run`, no body)\n\n" +
      "Wire types come from `horsie_models::routines::{RoutineView, RoutineRunResponse}` — already generated from `models/fluorite/routines.fl`, no model changes.\n\n" +
      "| Column | Meaning |\n|---|---|\n| NAME | the routine |\n| SCHEDULE | manual / every 3600s / once |\n",
  );

  await createSession(page, appBase, { model: "mock-sonnet" });
  await sendMessage(page, "explain the routines CLI design");
  await expectStatus(page, "Idle");
  await page.getByTestId("task-list-toggle").click();
  const sessionUrl = page.url();

  for (const skin of SKINS) {
    for (const mode of MODES) {
      await page.evaluate(
        ([s, m]) => {
          localStorage.setItem("horsie-skin", s!);
          localStorage.setItem("horsie-theme", m!);
        },
        [skin, mode],
      );
      await page.goto(sessionUrl);
      await page.waitForTimeout(800);
      await page.getByTestId("task-list-toggle").click().catch(() => {});

      await page.setViewportSize({ width: 1440, height: 900 });
      await page.screenshot({ path: `e2e/.shots/session-${skin}-${mode}.png` });

      await page.setViewportSize({ width: 390, height: 844 });
      await page.waitForTimeout(300);
      await page.screenshot({ path: `e2e/.shots/session-mobile-${skin}-${mode}.png` });
      await page.setViewportSize({ width: 1440, height: 900 });

      for (const [name, path] of [
        ["appearance", "/settings/appearance"],
        ["models", "/settings/models"],
        ["cards", "/admin/model-cards"],
        ["newsession", "/"],
        ["agent", "/agents/new"],
      ] as const) {
        await page.goto(`${appBase}${path}`);
        await page.waitForTimeout(500);
        await page.screenshot({ path: `e2e/.shots/${name}-${skin}-${mode}.png` });
      }
    }
  }
});
