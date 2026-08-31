import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AgentView, RoutineInput, RoutineView } from "../../api/types";
import { RoutineEditPage } from "./RoutineEditPage";

afterEach(cleanup);

const create = vi.fn(async (body: RoutineInput) => body);
const update = vi.fn(async (_name: string, body: RoutineInput) => body);
const browserZone = Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
const storedTimezone =
  browserZone === "America/New_York" ? "America/Los_Angeles" : "America/New_York";
const changedTimezone = "Asia/Tokyo";
const storedRoutine: RoutineView = {
  name: "stored",
  description: "",
  target: { type: "Agent", value: { agent: "reviewer" } },
  environment: { type: "Runtime", value: { vendor: "local" } },
  prompt: "triage the queue",
  schedule: {
    type: "Daily",
    value: { timezone: storedTimezone, hour: 8, minute: 30 },
  },
  enabled: true,
  createdAt: "1",
  updatedAt: "1",
};

beforeEach(() => {
  create.mockReset();
  update.mockReset();
});

vi.mock("../../api/client", () => ({
  api: {
    agents: {
      list: async (): Promise<AgentView[]> => [
        {
          name: "reviewer",
          description: "",
          model: "sonnet",
          plugins: [],
          mcpServers: [],
          memorySpaces: [],
          createdAt: "1",
          updatedAt: "1",
        },
      ],
    },
    routines: {
      get: async (name: string) => (name === storedRoutine.name ? storedRoutine : undefined),
      create: (body: RoutineInput) => create(body),
      update: (name: string, body: RoutineInput) => update(name, body),
    },
    workflows: {
      list: async () => [
        {
          name: "nightly-release",
          description: "",
          start: "build",
          steps: [{ name: "build", agent: "reviewer", prompt: "build" }],
          createdAt: "1",
          updatedAt: "1",
        },
      ],
    },
    // The form carries an environment field now, which reads all three.
    environments: { list: async () => [] },
    github: { status: async () => ({ connected: false, appConfigured: false, repoCount: 0 }) },
    config: {
      get: async () => ({
        providers: [],
        models: [],
        vendors: [
          { name: "local", isDefault: true, capabilities: { supportsProvisioning: false } },
        ],
        defaultRuntimeVendor: "local",
        info: {
          configPath: "",
          database: "",
          stateDir: "",
          dataDir: "",
          pluginsDir: "",
          version: "0",
        },
        restartRequired: false,
      }),
    },
  },
  ApiRequestError: class extends Error {},
}));

function renderNew() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const utils = render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={["/routines/new"]}>
        <Routes>
          <Route path="/routines/new" element={<RoutineEditPage />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return utils;
}

function renderStored() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const utils = render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={["/routines/stored/edit"]}>
        <Routes>
          <Route path="/routines/:name/edit" element={<RoutineEditPage />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return utils;
}

/** Pick the local runtime through the Environment field — a routine cannot be
 * saved without one, which is the point. */
async function chooseEnvironment(utils: ReturnType<typeof renderNew>) {
  fireEvent.click(utils.getByTestId("config-environment"));
  const option = await utils.findByTestId("environment-option");
  fireEvent.click(option);
}

describe("RoutineEditPage", () => {
  it("blocks saving until an environment is chosen", async () => {
    const utils = renderNew();
    fireEvent.change(await utils.findByTestId("routine-name-input"), {
      target: { value: "morning" },
    });
    fireEvent.change(utils.getByTestId("routine-agent-select"), {
      target: { value: "reviewer" },
    });
    fireEvent.change(utils.getByTestId("routine-prompt-input"), {
      target: { value: "triage the queue" },
    });
    const save = utils.getByTestId("save-routine-button") as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    await chooseEnvironment(utils);
    await waitFor(() => expect(save.disabled).toBe(false));

    fireEvent.click(save);
    await waitFor(() => expect(create).toHaveBeenCalledTimes(1));
    expect(create.mock.calls[0]?.[0].environment).toEqual({
      type: "Runtime",
      value: { vendor: "local", repos: undefined },
    });
  });

  it("defaults the timezone to the browser's and saves a daily schedule", async () => {
    const utils = renderNew();
    const { findByTestId, getByTestId, queryByTestId } = utils;
    fireEvent.change(await findByTestId("routine-name-input"), {
      target: { value: "morning" },
    });
    fireEvent.change(getByTestId("routine-agent-select"), {
      target: { value: "reviewer" },
    });
    fireEvent.change(getByTestId("routine-prompt-input"), {
      target: { value: "triage the queue" },
    });
    fireEvent.change(getByTestId("routine-schedule-kind"), {
      target: { value: "Daily" },
    });

    expect(queryByTestId("routine-timezone-select")).toBeNull();
    const timezoneToggle = getByTestId("routine-timezone-toggle");
    expect(timezoneToggle.getAttribute("aria-expanded")).toBe("false");
    expect(timezoneToggle.textContent).toContain("Change");
    fireEvent.click(timezoneToggle);
    expect(timezoneToggle.getAttribute("aria-expanded")).toBe("true");
    expect(getByTestId("routine-timezone-select")).not.toBeNull();

    const expectedZone = Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
    const zone = getByTestId("routine-timezone-select") as HTMLSelectElement;
    expect(zone.value).toBe(expectedZone);
    expect(zone.className).toContain("min-w-0");
    expect(zone.className).toContain("w-full");
    expect(zone.parentElement?.className).toContain("flex-col");

    fireEvent.change(getByTestId("routine-time-input"), {
      target: { value: "09:00" },
    });
    await chooseEnvironment(utils);
    fireEvent.click(getByTestId("save-routine-button"));

    await waitFor(() => expect(create).toHaveBeenCalledTimes(1));
    expect(create.mock.calls[0]?.[0].schedule).toEqual({
      type: "Daily",
      value: { timezone: expectedZone, hour: 9, minute: 0 },
    });
  });

  it("weekly requires at least one weekday before saving", async () => {
    const utils = renderNew();
    const { findByTestId, getByTestId } = utils;
    fireEvent.change(await findByTestId("routine-name-input"), {
      target: { value: "standup" },
    });
    fireEvent.change(getByTestId("routine-agent-select"), {
      target: { value: "reviewer" },
    });
    fireEvent.change(getByTestId("routine-prompt-input"), {
      target: { value: "summarize yesterday" },
    });
    fireEvent.change(getByTestId("routine-schedule-kind"), {
      target: { value: "Weekly" },
    });
    await chooseEnvironment(utils);

    const save = getByTestId("save-routine-button") as HTMLButtonElement;
    expect(save.disabled).toBe(true);

    const mon = getByTestId("weekday-mon") as HTMLButtonElement;
    const tue = getByTestId("weekday-tue") as HTMLButtonElement;
    expect(getByTestId("routine-weekdays").getAttribute("aria-label")).toBe(
      "Days of week",
    );
    expect(mon.getAttribute("aria-label")).toBe("Monday");
    expect(mon.getAttribute("aria-pressed")).toBe("false");
    expect(tue.getAttribute("aria-pressed")).toBe("false");

    fireEvent.click(mon);
    expect(mon.getAttribute("aria-pressed")).toBe("true");
    expect(mon.getAttribute("aria-pressed")).toBe("true");
    expect(tue.getAttribute("aria-pressed")).toBe("false");
    expect(save.disabled).toBe(false);

    // Toggling off returns the chip to the unselected look.
    fireEvent.click(mon);
    expect(mon.getAttribute("aria-pressed")).toBe("false");
    expect(mon.getAttribute("aria-pressed")).toBe("false");
    expect(save.disabled).toBe(true);

    // Re-select so the weekly schedule is valid again before saving.
    fireEvent.click(mon);
    expect(save.disabled).toBe(false);
    fireEvent.click(save);
    await waitFor(() => expect(create).toHaveBeenCalledTimes(1));
    const payload = create.mock.calls[0]?.[0].schedule as {
      type: string;
      value: { weekdays: string[]; timezone: string; hour: number; minute: number };
    };
    expect(payload.type).toBe("Weekly");
    expect(payload.value.weekdays).toEqual(["Mon"]);
    expect(payload.value.hour).toBe(9);
  });

  it("preserves a changed timezone when editing a stored custom timezone", async () => {
    const { findByTestId, getByTestId, getByText, queryByTestId } = renderStored();
    await findByTestId("routine-edit-page");

    expect(queryByTestId("routine-timezone-select")).toBeNull();
    expect(getByText("Custom timezone")).not.toBeNull();

    fireEvent.click(getByTestId("routine-timezone-toggle"));
    const timezone = getByTestId("routine-timezone-select") as HTMLSelectElement;
    expect(timezone.value).toBe(storedTimezone);

    fireEvent.change(timezone, { target: { value: changedTimezone } });
    fireEvent.click(getByTestId("save-routine-button"));

    await waitFor(() => expect(update).toHaveBeenCalledTimes(1));
    expect(update.mock.calls[0]?.[0]).toBe("stored");
    expect(update.mock.calls[0]?.[1].schedule).toEqual({
      type: "Daily",
      value: { timezone: changedTimezone, hour: 8, minute: 30 },
    });
  });

  it("selects weekdays with the Weekdays preset", async () => {
    const { findByTestId, getByTestId } = renderNew();
    fireEvent.change(await findByTestId("routine-schedule-kind"), {
      target: { value: "Weekly" },
    });

    fireEvent.click(getByTestId("routine-weekdays-weekdays"));

    const weekdays = getByTestId("routine-weekdays");
    const preset = getByTestId("routine-weekdays-weekdays");
    expect(weekdays.contains(preset)).toBe(false);

    for (const day of ["mon", "tue", "wed", "thu", "fri"]) {
      expect(
        getByTestId(`weekday-${day}`).getAttribute("aria-pressed"),
      ).toBe("true");
    }
    for (const day of ["sat", "sun"]) {
      expect(
        getByTestId(`weekday-${day}`).getAttribute("aria-pressed"),
      ).toBe("false");
    }
  });
  // Save used to stay enabled directly beneath the client's own "the shortest
  // interval is 1 minute": the validity chain fell through to the *calendar*
  // rule, which an `Every` schedule does not use and which was trivially true.
  it("blocks saving an interval shorter than the minimum", async () => {
    const utils = renderNew();
    const { findByTestId, getByTestId } = utils;
    fireEvent.change(await findByTestId("routine-name-input"), {
      target: { value: "too-fast" },
    });
    fireEvent.change(getByTestId("routine-agent-select"), {
      target: { value: "reviewer" },
    });
    fireEvent.change(getByTestId("routine-prompt-input"), {
      target: { value: "poll" },
    });
    fireEvent.change(getByTestId("routine-schedule-kind"), {
      target: { value: "Every" },
    });
    await chooseEnvironment(utils);

    const save = getByTestId("save-routine-button") as HTMLButtonElement;
    fireEvent.change(getByTestId("routine-interval-minutes"), {
      target: { value: "0" },
    });
    expect(save.disabled).toBe(true);

    fireEvent.change(getByTestId("routine-interval-minutes"), {
      target: { value: "5" },
    });
    expect(save.disabled).toBe(false);
  });
});

describe("what a routine runs", () => {
  /**
   * A routine used to be an agent preset and nothing else, so the form had one
   * select and the wire had one string. Both are now a tagged choice: a
   * workflow and a preset are both slugs, and a single field could not say
   * which kind a name was.
   */
  it("saves a workflow target when the workflow kind is chosen", async () => {
    const utils = renderNew();
    const { getByTestId } = utils;
    fireEvent.change(await utils.findByTestId("routine-name-input"), {
      target: { value: "nightly" },
    });
    fireEvent.click(getByTestId("routine-target-workflow"));
    await waitFor(() => getByTestId("routine-workflow-select"));
    fireEvent.change(getByTestId("routine-workflow-select"), {
      target: { value: "nightly-release" },
    });
    fireEvent.change(getByTestId("routine-prompt-input"), {
      target: { value: "ship it" },
    });
    await chooseEnvironment(utils);
    fireEvent.click(getByTestId("save-routine-button"));

    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0]?.[0].target).toEqual({
      type: "Workflow",
      value: { workflow: "nightly-release" },
    });
  });

  /** Switching kind must not silently save the other one: the two selects hold
   * their own state, and only the chosen kind reaches the wire. */
  it("keeps the two kinds apart", async () => {
    const { getByTestId, queryByTestId } = renderNew();
    await waitFor(() => getByTestId("routine-agent-select"));
    expect(queryByTestId("routine-workflow-select")).toBeNull();
    fireEvent.click(getByTestId("routine-target-workflow"));
    expect(queryByTestId("routine-agent-select")).toBeNull();
    expect(getByTestId("routine-workflow-select")).toBeTruthy();
  });

  /** A saved routine opens on the kind it holds. */
  it("opens an agent routine on the agent kind", async () => {
    const { getByTestId } = renderStored();
    await waitFor(() =>
      expect(
        (getByTestId("routine-agent-select") as HTMLSelectElement).value,
      ).toBe("reviewer"),
    );
  });
});
