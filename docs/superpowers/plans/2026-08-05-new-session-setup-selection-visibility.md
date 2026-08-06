# New-session setup selection visibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the currently chosen options visibly highlighted inside the existing new-session picker menus without adding new UI.

**Architecture:** Keep `SessionConfigBar`, `PopoverMenu`, and the current picker layout unchanged. Add a shared selected-option presentation in `configPickers.tsx`: selected non-checkbox options receive the existing raised visual treatment, a checkmark, and explicit selection semantics; thinking effort keeps its native radio and receives the same row highlight. Tests inspect the picker option output rather than changing request/state behavior.

**Tech Stack:** React, TypeScript, Tailwind utility classes, Vitest, Testing Library, React Query.

## Global Constraints

- Do not add a summary section, new controls, labels, or duplicated setup information.
- Preserve picker ordering, menu behavior, selection callbacks, persistence, and request construction.
- Preserve the existing native radio controls for thinking effort.
- Leave checkbox list checked states unchanged.
- Follow the repository rule that protocol types are generated and unrelated to this UI change.

---

### Task 1: Add failing picker selection-state tests

**Files:**
- Modify: `clients/web/src/components/configPickers.test.tsx`
- Test: `clients/web/src/components/configPickers.test.tsx`

**Interfaces:**
- Consumes: `useConfigPickers(draft)` and each returned `PickerSpec.body(close)` renderer.
- Produces: regression tests requiring selected picker options to expose `data-selected="true"`, while unselected options expose `data-selected="false"` or omit the attribute.

- [ ] **Step 1: Add test imports and a small menu renderer helper**

Extend the existing Testing Library imports with `render`, and import `Fragment`/`ReactNode` only if needed by the existing JSX style. Add a helper that obtains a picker by key from `useConfigPickers`, renders its `body` inside a `MemoryRouter`/`QueryClientProvider`, and returns the rendered view. The helper must invoke the picker body with a no-op close callback:

```tsx
function renderPickerBody(draft: ConfigDraft, key: string) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  client.setQueryData(settingsKey, settings);
  const { result } = renderHook(() => useConfigPickers(draft), {
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={client}>
        <MemoryRouter>{children}</MemoryRouter>
      </QueryClientProvider>
    ),
  });
  const picker = result.current.find((p) => p.key === key);
  if (!picker) throw new Error(`missing picker ${key}`);
  return render(<>{picker.body(() => {})}</>, { wrapper: MemoryRouter });
}
```

If the local Testing Library version does not accept `MemoryRouter` directly as a wrapper, use an inline wrapper component returning `<MemoryRouter>{children}</MemoryRouter>`; do not alter production code to accommodate the test.

- [ ] **Step 2: Add failing tests for runtime and model**

Add tests using `sessionDraft` that set `vendor: "local"` and `model: "sonnet"`, then assert the chosen option has `data-selected="true"` and a selected styling class, while an available unchosen option does not. Expand the fixture settings only within the test if a second model/vendor is needed:

```tsx
it("marks the selected runtime option", () => {
  const view = renderPickerBody(sessionDraft({ vendor: "local" }), "runtime");
  expect(view.getByTestId("runtime-option")).toHaveAttribute("data-selected", "true");
});
```

Use a second runtime/model fixture so the test distinguishes selected from unselected rows; retain the existing `settings` fixture as the source of truth and add only the minimum alternate option.

- [ ] **Step 3: Add failing tests for workflow and thinking effort**

For a workflow draft with `workflow: "triage"`, assert the `triage` option is selected and the `None` option is not. For a draft with `thinkingEfforts: ["low", "high"]` and `thinkingEffort: "high"`, assert the `high` row is selected and its radio remains checked. The test must verify that the native radio behavior is preserved, not replaced.

- [ ] **Step 4: Run the focused tests and verify failure**

Run from `clients/web`:

```bash
npm test -- --run src/components/configPickers.test.tsx
```

Expected: the existing picker-list tests pass, and the new selection-state tests fail because the option rows do not yet expose selected state.

- [ ] **Step 5: Commit the failing tests**

```bash
git add clients/web/src/components/configPickers.test.tsx
git commit -m "test: cover session setup picker selection state"
```

### Task 2: Implement selected option highlighting in existing menus

**Files:**
- Modify: `clients/web/src/components/configPickers.tsx`
- Test: `clients/web/src/components/configPickers.test.tsx`

**Interfaces:**
- Consumes: current draft values (`vendor`, `model`, `workflow`, `thinkingEffort`) and existing picker option callbacks.
- Produces: option rows with a consistent selected visual state and `data-selected`/ARIA state, without changing picker behavior.

- [ ] **Step 1: Add a shared selected-row class constant/helper**

Near the existing picker helpers, define a small presentation helper or constant for option rows. It must preserve the current rounded spacing and hover behavior while adding the existing raised selected treatment:

```tsx
const optionClass = (selected: boolean) =>
  cn(
    "flex w-full items-center gap-2 rounded-[var(--radius-chip)] px-2 py-1.5 text-left text-sm",
    selected ? "bg-raised text-legend" : "hover:bg-raised",
  );
```

Import and use the repository’s existing `cn` helper rather than concatenating conditionals manually. Keep the helper local to `configPickers.tsx`; do not create a new component for a one-line row treatment.

- [ ] **Step 2: Apply selected state to workflow options**

Set `selected` by comparing `d.workflow` with each option value. Mark `None` selected when `d.workflow === ""`; mark a named workflow selected when it matches. Add `data-selected={selected}` and `aria-pressed={selected}` to each workflow button. Render a right-aligned checkmark only for selected options, with `aria-hidden` set on the icon.

- [ ] **Step 3: Apply selected state to runtime options**

Compare `v.name` with `d.vendor`. Use the shared selected row class, `data-selected`, and `aria-pressed`. Preserve the existing `default` text and selection callback. Add a checkmark after the existing content, using `ml-auto` so it does not disturb the runtime name/default layout.

- [ ] **Step 4: Apply selected state to model options**

Compare `m.alias` with `draft.model`. Preserve the model alias and model ID two-line layout. Put the selected marker in a row aligned with the alias, and use `data-selected` plus `aria-pressed`. Keep the existing click callback and menu close behavior exactly as-is.

- [ ] **Step 5: Apply selected state to thinking effort while retaining radios**

Keep the existing `input type="radio"`, `name`, `checked`, and `onChange` behavior. Compute whether each row is selected using the same effective value logic already represented by the radio’s `checked` state (`draft.thinkingEffort === ""` for default, or equality for a named effort). Apply the shared selected row class and `data-selected` to the labels. Do not add a second checkmark or replace the radio.

- [ ] **Step 6: Run the focused tests and verify they pass**

Run:

```bash
npm test -- --run src/components/configPickers.test.tsx
```

Expected: all picker-list and new selected-state tests pass.

- [ ] **Step 7: Run formatting/type checks for the web client**

Run:

```bash
npm run typecheck
npm run lint
npm run format:check
```

Expected: all commands pass. If the package scripts use different names, inspect `clients/web/package.json` and run the equivalent existing scripts; do not add new scripts.

- [ ] **Step 8: Commit the implementation**

```bash
git add clients/web/src/components/configPickers.tsx clients/web/src/components/configPickers.test.tsx
git commit -m "feat: highlight selected session setup options"
```

### Task 3: Verify the end-to-end setup review behavior

**Files:**
- Test: `clients/web/e2e/j-new-session.spec.ts`

**Interfaces:**
- Consumes: the picker `data-selected` contract from Task 2.
- Produces: browser coverage that opening the new-session setup menus visibly identifies the current runtime and model choices.

- [ ] **Step 1: Add an E2E regression test for selected runtime/model options**

Use the existing `mock.reset`, `createSession`, and page setup patterns. Open the new-session draft, open the runtime picker, and assert the current `runtime-option[data-selected="true"]` contains the configured runtime. Then close it, open the model picker, and assert the current `model-option[data-selected="true"]` contains the configured model alias. Avoid asserting exact colors; assert the stable selected-state attribute and visible text.

- [ ] **Step 2: Run the focused E2E test**

From `clients/web` run:

```bash
npx playwright test e2e/j-new-session.spec.ts
```

Expected: all J-group tests pass, including the new selected-option test.

- [ ] **Step 3: Run repository verification**

From the repository root run:

```bash
cargo fmt --check
cargo test --workspace
```

Also rerun the web focused unit and E2E commands from the previous tasks if the web client is not included in the workspace commands. Report any environment-specific unavailable command rather than claiming it passed.

- [ ] **Step 4: Inspect the final diff and status**

Run:

```bash
git diff origin/main...HEAD --check
git status --short
```

Expected: no whitespace errors, only the design spec and the targeted web implementation/tests are changed, and the worktree is clean.

- [ ] **Step 5: Commit any final test-only adjustment if required**

If the E2E test requires a small selector correction discovered during execution, commit only that targeted adjustment:

```bash
git add clients/web/e2e/j-new-session.spec.ts
git commit -m "test: verify selected session setup options"
```
