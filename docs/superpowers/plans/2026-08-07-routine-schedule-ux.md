# Routine Schedule UX Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make weekly routine day selection read as a clear grouped control and progressively disclose timezone editing while preserving the existing schedule API and behavior.

**Architecture:** Keep the existing `RoutineForm` state and `buildSchedule` payload construction intact. Add only local presentation state for timezone disclosure, and express the day selector through existing Console utility classes and native button semantics. Extend the colocated form tests to cover collapsed/revealed timezone editing, accessible weekday selection, and the unchanged saved payload.

**Tech Stack:** React 19, TypeScript, Tailwind v4 utility classes, Testing Library, Vitest, Playwright, Vite.

## Global Constraints

- Preserve the existing `RoutineSchedule` payload shapes and canonical Mon–Sun server order.
- `browserTimezone()` remains the default for new calendar schedules; stored edit values remain unchanged.
- Safety orange remains reserved for the form’s commit action; selected weekdays use the existing amber semantic selection treatment.
- Use native buttons/selects with visible focus and accessible names; do not introduce a new component or dependency.
- Keep changes local to `clients/web/src/pages/routines/RoutineEditPage.tsx` and its colocated test unless verification exposes a necessary narrow fix.
- Use existing horsie Console tokens and radii; no raw colors, new CSS tokens, or rounded pills.

---

### Task 1: Lock the new schedule interactions in focused tests

**Files:**
- Modify: `clients/web/src/pages/routines/RoutineEditPage.test.tsx`

**Interfaces:**
- Consumes: Existing `renderNew()`, mocked routine creation function, and `RoutineEditPage` test ids.
- Produces: Regression coverage for the collapsed timezone disclosure and accessible grouped weekday selector that the implementation must satisfy.

- [ ] **Step 1: Update the daily timezone test to assert the collapsed default**

Destructure `queryByTestId` from `renderNew()`. After changing the schedule kind to `Daily`, add:

```ts
expect(queryByTestId("routine-timezone-select")).toBeNull();
const timezoneToggle = getByTestId("routine-timezone-toggle");
expect(timezoneToggle).toHaveAttribute("aria-expanded", "false");
expect(timezoneToggle).toHaveTextContent("Change");
```

- [ ] **Step 2: Add the timezone reveal assertion**

Immediately after the collapsed assertions, click the toggle and verify the select is mounted and the disclosure state changed:

```ts
fireEvent.click(timezoneToggle);
expect(timezoneToggle).toHaveAttribute("aria-expanded", "true");
expect(getByTestId("routine-timezone-select")).not.toBeNull();
```

Continue saving after the reveal so the existing payload assertion proves the timezone value remains the browser timezone.

- [ ] **Step 3: Strengthen weekly selected-state assertions**

For the weekday test, replace the orange-class expectations with semantic and visual-state checks:

```ts
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
```

After toggling off, assert `aria-pressed="false"` and that `border-amber` is absent. Keep the save-disabled and payload assertions.

- [ ] **Step 4: Add coverage for the Weekdays preset**

Render a new weekly form, choose `Weekly`, click `routine-weekdays-weekdays`, and assert Mon–Fri have `aria-pressed="true"` while Sat/Sun remain false. This test only exercises the presentation state and does not need to fill or save the rest of the form.

- [ ] **Step 5: Run the focused test and verify it fails for the missing UI**

Run:

```bash
npm --prefix clients/web run test:unit -- src/pages/routines/RoutineEditPage.test.tsx
```

Expected: FAIL because the timezone toggle, collapsed select, grouped weekday test id, and new selected classes do not exist yet.

- [ ] **Step 6: Commit the test-first changes**

```bash
git add clients/web/src/pages/routines/RoutineEditPage.test.tsx
git commit -m "test(web): define routine schedule ux states"
```

---

### Task 2: Implement the timezone disclosure and weekday selector

**Files:**
- Modify: `clients/web/src/pages/routines/RoutineEditPage.tsx`

**Interfaces:**
- Consumes: Existing schedule state, `browserTimezone()`, `timezoneOptions()`, `WEEKDAY_ORDER`, and the test contract from Task 1.
- Produces: A closed-by-default timezone editor and accessible segmented weekday selector with unchanged schedule payloads.

- [ ] **Step 1: Add local timezone disclosure state and browser-zone comparison**

Inside `RoutineForm`, after the existing `timezone` state, add:

```ts
const localTimezone = browserTimezone();
const [timezoneEditorOpen, setTimezoneEditorOpen] = useState(false);
```

Keep timezone initialization exactly as it is so existing stored timezones are preserved and new schedules default to the browser zone.

- [ ] **Step 2: Replace the always-visible timezone select with a summary and disclosure**

Within the calendar schedule branch, keep the time input and replace the inline timezone select with:

```tsx
<div className="flex flex-wrap items-center gap-2 text-xs text-dim">
  <span>
    {timezone === localTimezone ? "Browser timezone" : "Custom timezone"}
    <span className="ml-1 font-mono text-faint">· {timezone}</span>
  </span>
  <button
    type="button"
    className="key-flat !px-1.5 !py-1 text-xs"
    aria-controls="routine-timezone-editor"
    aria-expanded={timezoneEditorOpen}
    data-testid="routine-timezone-toggle"
    onClick={() => setTimezoneEditorOpen((open) => !open)}
  >
    {timezoneEditorOpen ? "Done" : "Change"}
  </button>
</div>
{timezoneEditorOpen && (
  <div id="routine-timezone-editor" className="mt-2">
    <label className="flex items-center gap-2 text-xs text-dim">
      <span>Timezone</span>
      <select
        className="field field-mono"
        value={timezone}
        onChange={(e) => setTimezone(e.target.value)}
        data-testid="routine-timezone-select"
      >
        {timezoneOptions().map((z) => (
          <option key={z} value={z}>{z}</option>
        ))}
      </select>
    </label>
  </div>
)}
```

The `aria-controls` target is the revealed editor wrapper, and the visible label keeps the select understandable when opened. Keep the summary in the same flex group as the time control, but allow it to wrap on narrow widths.

- [ ] **Step 3: Add full weekday names and build the named segmented group**

Define a module-level record keyed by the `Weekday` enum values:

```ts
const FULL_WEEKDAY_NAMES: Record<Weekday, string> = {
  [Weekday.Mon]: "Monday",
  [Weekday.Tue]: "Tuesday",
  [Weekday.Wed]: "Wednesday",
  [Weekday.Thu]: "Thursday",
  [Weekday.Fri]: "Friday",
  [Weekday.Sat]: "Saturday",
  [Weekday.Sun]: "Sunday",
};
```

Replace the current weekday chip row with:

```tsx
<div
  className="flex flex-wrap items-center gap-1.5"
  role="group"
  aria-label="Days of week"
  data-testid="routine-weekdays"
>
  {WEEKDAY_ORDER.map((d) => (
    <button
      key={d}
      type="button"
      className={`chip min-h-10 min-w-10 justify-center transition-colors ${
        weekdays.has(d)
          ? "border-amber bg-amber/15 text-amber-ink shadow-[inset_0_0_0_1px_var(--amber)]"
          : "hover:bg-raised hover:text-legend"
      }`}
      aria-label={FULL_WEEKDAY_NAMES[d]}
      aria-pressed={weekdays.has(d)}
      onClick={() =>
        setWeekdays((prev) => {
          const next = new Set(prev);
          if (next.has(d)) next.delete(d);
          else next.add(d);
          return next;
        })
      }
      data-testid={`weekday-${d.toLowerCase()}`}
    >
      {d}
    </button>
  ))}
  <button
    type="button"
    className="key-flat !px-2 !py-1.5 text-xs"
    onClick={() => setWeekdays(new Set(WEEKDAY_ORDER.slice(0, 5)))}
    data-testid="routine-weekdays-weekdays"
  >
    Weekdays
  </button>
</div>
```

Preserve the existing validation message and server ordering.

- [ ] **Step 4: Run the focused unit test and fix only implementation mismatches**

Run:

```bash
npm --prefix clients/web run test:unit -- src/pages/routines/RoutineEditPage.test.tsx
```

Expected: PASS with the daily timezone payload, closed/revealed editor, weekly validation, selected state, toggle-off behavior, and Weekdays preset.

- [ ] **Step 5: Commit the implementation**

```bash
git add clients/web/src/pages/routines/RoutineEditPage.tsx
git commit -m "fix(web): clarify routine schedule controls"
```

---

### Task 3: Verify the complete web surface and mechanical quality

**Files:**
- Inspect: `clients/web/src/pages/routines/RoutineEditPage.tsx`
- Inspect: `clients/web/src/pages/routines/RoutineEditPage.test.tsx`
- Inspect: `clients/web/dist` only through build output; do not commit generated artifacts.

**Interfaces:**
- Consumes: The implementation and tests from Tasks 1–2.
- Produces: A verified branch ready for review and PR creation.

- [ ] **Step 1: Run the focused unit test from the final implementation**

```bash
npm --prefix clients/web run test:unit -- src/pages/routines/RoutineEditPage.test.tsx
```

Expected: PASS.

- [ ] **Step 2: Run the web typecheck and production build**

```bash
npm --prefix clients/web run typecheck
npm --prefix clients/web run build
```

Expected: both commands exit 0 with no TypeScript diagnostics or Vite build errors.

- [ ] **Step 3: Run the Impeccable detector once over the changed UI file**

```bash
node /Users/xiaoguang/.local/state/horsie/plugins/3fce35cd-3623-45ba-bfa9-7ee47809174c/impeccable/skills/impeccable/scripts/detect.mjs --json clients/web/src/pages/routines/RoutineEditPage.tsx
```

Review findings in context; fix only real findings before the final verification pass. Do not introduce raw colors, pill controls, or a second orange commit control.

- [ ] **Step 4: Run the routine Playwright spec if its required harness is available**

```bash
npm --prefix clients/web run test:e2e -- e2e/q-routines.spec.ts
```

Expected: PASS. If the fixture cannot boot in this environment, record the exact harness failure and rely on the focused unit/build checks rather than claiming e2e coverage.

- [ ] **Step 5: Inspect the final diff for scope and formatting**

```bash
git diff origin/main...HEAD --check
git diff origin/main...HEAD --stat
git status --short
```

Expected: only the approved spec, routine form test, routine form implementation, and this plan are changed; no generated build output or temporary files remain.

- [ ] **Step 6: Commit any narrowly scoped verification fix**

If verification requires a code fix, run the focused test again, then commit it with:

```bash
git add clients/web/src/pages/routines/RoutineEditPage.tsx clients/web/src/pages/routines/RoutineEditPage.test.tsx
git commit -m "fix(web): polish routine schedule settings"
```

Otherwise leave the test and implementation commits intact and proceed to the PR review workflow.
