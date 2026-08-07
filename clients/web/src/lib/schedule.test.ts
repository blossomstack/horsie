import { describe, expect, it } from "vitest";
import type { RoutineSchedule } from "../api/types";
import { Weekday } from "../api/types";
import { describeSchedule, timezoneOptions } from "./schedule";

function schedule(s: RoutineSchedule) {
  return describeSchedule(s);
}

describe("describeSchedule", () => {
  it("describes the manual arm", () => {
    expect(schedule({ type: "Manual", value: {} })).toBe("manually");
  });

  it("describes every by the coarsest whole unit", () => {
    expect(
      schedule({ type: "Every", value: { intervalSecs: 3600 } }),
    ).toBe("every 1h");
  });

  it("describes once as a local instant", () => {
    const s = schedule({ type: "Once", value: { atMs: 0 } });
    expect(s).toMatch(/^once on /);
  });

  it("describes daily with time and zone", () => {
    expect(
      schedule({
        type: "Daily",
        value: { timezone: "Asia/Shanghai", hour: 9, minute: 5 },
      }),
    ).toBe("daily at 09:05 (Asia/Shanghai)");
  });

  it("describes weekly with its days", () => {
    expect(
      schedule({
        type: "Weekly",
        value: {
          timezone: "Asia/Shanghai",
          hour: 9,
          minute: 0,
          weekdays: [Weekday.Mon, Weekday.Wed, Weekday.Fri],
        },
      }),
    ).toBe("every Mon, Wed, Fri at 09:00 (Asia/Shanghai)");
  });

  it("describes monthly with an ordinal day", () => {
    expect(
      schedule({
        type: "Monthly",
        value: { timezone: "UTC", hour: 9, minute: 0, dayOfMonth: 15 },
      }),
    ).toBe("monthly on the 15th at 09:00 (UTC)");
  });

  it("describes yearly with month and day", () => {
    expect(
      schedule({
        type: "Yearly",
        value: { timezone: "UTC", hour: 9, minute: 0, month: 2, dayOfMonth: 15 },
      }),
    ).toBe("yearly on Feb 15th at 09:00 (UTC)");
  });
});

describe("timezoneOptions", () => {
  it("always includes the browser's timezone", () => {
    const zones = timezoneOptions();
    const browser = Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
    expect(zones).toContain(browser);
    expect([...zones].sort()).toEqual(zones);
  });
});
