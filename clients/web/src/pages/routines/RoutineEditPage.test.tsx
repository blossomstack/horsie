import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AgentView, RoutineInput } from "../../api/types";
import { RoutineEditPage } from "./RoutineEditPage";

afterEach(cleanup);

const create = vi.fn(async (body: RoutineInput) => body);

beforeEach(() => create.mockReset());

vi.mock("../../api/client", () => ({
  api: {
    agents: {
      list: async (): Promise<AgentView[]> => [
        {
          name: "reviewer",
          description: "",
          model: "sonnet",
          repos: [],
          plugins: [],
          mcpServers: [],
          memorySpaces: [],
          createdAt: "1",
          updatedAt: "1",
        },
      ],
    },
    routines: {
      get: async () => undefined,
      create: (body: RoutineInput) => create(body),
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

describe("RoutineEditPage", () => {
  it("defaults the timezone to the browser's and saves a daily schedule", async () => {
    const { findByTestId, getByTestId, queryByTestId } = renderNew();
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
    expect(timezoneToggle).toHaveAttribute("aria-expanded", "false");
    expect(timezoneToggle).toHaveTextContent("Change");
    fireEvent.click(timezoneToggle);
    expect(timezoneToggle).toHaveAttribute("aria-expanded", "true");
    expect(getByTestId("routine-timezone-select")).not.toBeNull();

    const expectedZone = Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
    const zone = getByTestId("routine-timezone-select") as HTMLSelectElement;
    expect(zone.value).toBe(expectedZone);

    fireEvent.change(getByTestId("routine-time-input"), {
      target: { value: "09:00" },
    });
    fireEvent.click(getByTestId("save-routine-button"));

    await waitFor(() => expect(create).toHaveBeenCalledTimes(1));
    expect(create.mock.calls[0]?.[0].schedule).toEqual({
      type: "Daily",
      value: { timezone: expectedZone, hour: 9, minute: 0 },
    });
  });

  it("weekly requires at least one weekday before saving", async () => {
    const { findByTestId, getByTestId } = renderNew();
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

    const save = getByTestId("save-routine-button") as HTMLButtonElement;
    expect(save.disabled).toBe(true);

    const mon = getByTestId("weekday-mon") as HTMLButtonElement;
    const tue = getByTestId("weekday-tue") as HTMLButtonElement;
    expect(getByTestId("routine-weekdays")).toHaveAttribute(
      "aria-label",
      "Days of week",
    );
    expect(mon).toHaveAttribute("aria-label", "Monday");
    expect(mon).toHaveAttribute("aria-pressed", "false");
    expect(tue).toHaveAttribute("aria-pressed", "false");

    fireEvent.click(mon);
    expect(mon).toHaveAttribute("aria-pressed", "true");
    expect(mon.className).toContain("border-amber");
    expect(mon.className).toContain("bg-amber/15");
    expect(tue).toHaveAttribute("aria-pressed", "false");
    expect(save.disabled).toBe(false);

    // Toggling off returns the chip to the unselected look.
    fireEvent.click(mon);
    expect(mon).toHaveAttribute("aria-pressed", "false");
    expect(mon.className).not.toContain("border-amber");
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

  it("selects weekdays with the Weekdays preset", async () => {
    const { findByTestId, getByTestId } = renderNew();
    fireEvent.change(await findByTestId("routine-schedule-kind"), {
      target: { value: "Weekly" },
    });

    fireEvent.click(getByTestId("routine-weekdays-weekdays"));

    for (const day of ["mon", "tue", "wed", "thu", "fri"]) {
      expect(getByTestId(`weekday-${day}`)).toHaveAttribute(
        "aria-pressed",
        "true",
      );
    }
    for (const day of ["sat", "sun"]) {
      expect(getByTestId(`weekday-${day}`)).toHaveAttribute(
        "aria-pressed",
        "false",
      );
    }
  });
});
