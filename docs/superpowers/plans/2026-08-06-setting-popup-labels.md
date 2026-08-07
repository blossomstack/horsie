# Setting Popup Labels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show each setting's existing name at the top of icon-style popups on the new-session screen, matching the readouts after session creation.

**Architecture:** Centralize the visual popup heading in `PopoverMenu` for the `icon` variant, using the existing `legend` prop already supplied by both draft pickers and locked readouts. Remove the redundant heading from locked readout content so both surfaces render one shared heading without changing picker data, behavior, or accessibility labels.

**Tech Stack:** React 19, TypeScript, Tailwind CSS, Testing Library, Vitest.

## Global Constraints

- Use the existing `PickerSpec.legend` strings as the setting names; do not add a second title vocabulary.
- Presentation-only: do not change APIs, persistence, state behavior, option values, popup placement, or field-style controls.
- Preserve existing trigger tooltips and accessible names, which are derived from `legend` and `label`.
- Follow the repository's TypeScript, React, and colocated Vitest test conventions.

---

### Task 1: Centralize icon-popup headings and cover draft/locked consistency

**Files:**
- Modify: `clients/web/src/components/PopoverMenu.tsx` — render `legend` at the top of open `icon` popup content.
- Modify: `clients/web/src/components/configPickers.tsx` — remove the readout helper's now-redundant visible heading while retaining its values.
- Modify: `clients/web/src/components/SessionConfigBar.test.tsx` — verify the locked Model readout does not duplicate its name.
- Create: `clients/web/src/components/PopoverMenu.test.tsx` — verify an icon popup displays its legend above its content.

**Interfaces:**
- Consumes: Existing `PopoverMenu` props (`variant`, `legend`, `children`) and existing `PickerSpec.legend` values.
- Produces: Icon-style popups that visibly render one `legend` heading before their children; field-style triggers remain unchanged.

- [ ] **Step 1: Write the failing unit test for an icon popup heading**

Create `clients/web/src/components/PopoverMenu.test.tsx` using the same `@testing-library/react`, `vitest`, and cleanup conventions as the existing component tests. Render an icon popup with a legend and a uniquely identifiable option, click its trigger, and assert the popup contains the legend and option in document order:

```tsx
import { cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { PopoverMenu } from "./PopoverMenu";

afterEach(cleanup);

describe("PopoverMenu icon variant", () => {
  it("shows the setting name above the popup content", () => {
    const { getByTestId, getByText } = render(
      <PopoverMenu
        variant="icon"
        legend="Model"
        label="sonnet"
        testId="config-model"
      >
        {() => <div data-testid="model-options">sonnet</div>}
      </PopoverMenu>,
    );

    fireEvent.click(getByTestId("config-model"));

    const heading = getByText("Model");
    const options = getByTestId("model-options");
    expect(heading).toBeTruthy();
    expect(heading.compareDocumentPosition(options) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });
});
```

- [ ] **Step 2: Run the focused test and verify it fails**

Run from `clients/web`:

```bash
npm run test:unit -- src/components/PopoverMenu.test.tsx
```

Expected: FAIL because opening the icon popup currently renders only the children and no visible `Model` heading.

- [ ] **Step 3: Add the shared popup heading**

In `PopoverMenu.tsx`, inside the existing `{open && !disabled && (...)}` panel, render a heading wrapper before `children`. Use the existing `legend` only for icon popups, so field-style controls are untouched:

```tsx
{variant === "icon" && legend && (
  <p className="legend px-1 pb-1.5">{legend}</p>
)}
{children(() => setOpen(false))}
```

Keep the heading inside the existing scrollable panel and preserve the existing `p-1.5` panel padding. Do not alter `described`, trigger markup, placement classes, width, or state handling.

- [ ] **Step 4: Remove the locked-readout duplicate heading**

In `configPickers.tsx`, change `readout` to accept only `items: string[]` and render only the readout values in the same padded container:

```tsx
const readout = (items: string[]) => () => (
  <div className="space-y-1.5 px-1 py-0.5">
    {items.length === 0 ? (
      <p className="text-sm text-faint">None</p>
    ) : (
      <ul className="space-y-0.5">
        {items.map((v) => (
          <li key={v} className="font-mono text-[0.8125rem] break-words text-legend">
            {v}
          </li>
        ))}
      </ul>
    )}
  </div>
);
```

Update every `readout(...)` call to pass only its item array. The popup-level `PopoverMenu` heading is now the sole visible name.

- [ ] **Step 5: Extend the locked-session test for exactly one heading**

In `SessionConfigBar.test.tsx`, add `fireEvent` to the Testing Library import and add a test that opens the locked model key:

```tsx
it("shows the locked setting name once in the model popup", () => {
  const { getByTestId, getAllByText } = renderLocked(detail());
  fireEvent.click(getByTestId("config-model"));
  expect(getAllByText("Model")).toHaveLength(1);
});
```

This verifies centralizing the heading did not produce duplicate names in the existing-session readout. Keep the current accessible-name tests unchanged.

- [ ] **Step 6: Run focused unit tests and verify they pass**

Run:

```bash
npm run test:unit -- src/components/PopoverMenu.test.tsx src/components/SessionConfigBar.test.tsx
```

Expected: PASS with all tests in both files passing.

- [ ] **Step 7: Run typecheck and the complete web unit suite**

Run:

```bash
npm run typecheck
npm run test:unit
```

Expected: both commands exit successfully with no TypeScript errors or unit-test failures.

- [ ] **Step 8: Review the diff and commit**

Run:

```bash
git diff --check
git diff -- clients/web/src/components/PopoverMenu.tsx clients/web/src/components/configPickers.tsx clients/web/src/components/PopoverMenu.test.tsx clients/web/src/components/SessionConfigBar.test.tsx
git status --short
```

Confirm only the planned component/test files changed, then commit:

```bash
git add clients/web/src/components/PopoverMenu.tsx clients/web/src/components/configPickers.tsx clients/web/src/components/PopoverMenu.test.tsx clients/web/src/components/SessionConfigBar.test.tsx
git commit -m "fix: label setting popups before session creation"
```
