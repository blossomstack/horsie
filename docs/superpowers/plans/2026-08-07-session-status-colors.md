# Session Status Color Hierarchy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Make running sessions visibly active in the existing amber live color while rendering idle sessions with a subdued neutral color.

**Architecture:** Keep status semantics centralized in `clients/web/src/lib/status.ts`. Add an explicit `idle` tone mapped to the existing `text-dim` utility; keep `Running` on `live` with `busy: true`, so `StatusDot` applies the same tone to the animated lamp and status text without component changes.

**Tech Stack:** React, TypeScript, Vitest, Bun/npm package scripts.

## Global Constraints

- Preserve the existing status labels, hints, sendability, animation timing, and all non-Idle status behavior.
- Keep `off` reserved for unknown/unloaded status.
- Do not add new CSS color tokens when the existing `text-dim` utility satisfies the subdued Idle presentation.
- Follow the repository convention of colocated unit tests under `clients/web/src`.

---

### Task 1: Add regression coverage for the status color hierarchy

**Files:**
- Create: `clients/web/src/lib/status.test.ts`
- Read: `clients/web/src/lib/status.ts`
- Read: `clients/web/src/api/types.ts`

**Interfaces:**
- Consumes: `statusMeta(status)`, `TONE_TEXT`, `SessionStatusKind`, and the returned metadata fields `tone` and `busy`.
- Produces: Regression tests that constrain the semantic tone mapping before the implementation changes.

- [x] **Step 1: Write the failing tests**

Create `clients/web/src/lib/status.test.ts` with Vitest tests for the requested mapping:

```ts
import { describe, expect, it } from "vitest";
import { SessionStatusKind } from "../api/types";
import { statusMeta, TONE_TEXT } from "./status";

describe("status presentation metadata", () => {
  it("keeps Running prominent, amber, and animated", () => {
    const meta = statusMeta(SessionStatusKind.Running);

    expect(meta.tone).toBe("live");
    expect(meta.busy).toBe(true);
    expect(TONE_TEXT[meta.tone]).toBe("text-amber-ink");
  });

  it("renders Idle with a subdued neutral tone and no animation", () => {
    const meta = statusMeta(SessionStatusKind.Idle);

    expect(meta.tone).toBe("idle");
    expect(meta.busy).toBe(false);
    expect(TONE_TEXT[meta.tone]).toBe("text-dim");
  });

  it("keeps an unknown status separate from Idle", () => {
    expect(statusMeta(undefined).tone).toBe("off");
    expect(statusMeta(null).tone).toBe("off");
  });
});
```

- [x] **Step 2: Run the focused test and verify it fails**

Run from the worktree root:

```bash
cd clients/web && npm run test:unit -- src/lib/status.test.ts
```

Expected: FAIL because `StatusTone` does not yet include `idle` and Idle currently resolves to `ready`.

- [x] **Step 3: Commit the failing test**

```bash
git add clients/web/src/lib/status.test.ts
git commit -m "test: specify session status color hierarchy"
```

### Task 2: Implement the semantic Idle tone

**Files:**
- Modify: `clients/web/src/lib/status.ts`

**Interfaces:**
- Consumes: Existing `StatusTone`, `META`, and `TONE_TEXT` definitions.
- Produces: `statusMeta(SessionStatusKind.Idle)` with `tone: "idle"` and `TONE_TEXT.idle` equal to `"text-dim"`; Running remains `tone: "live"`, `busy: true`.

- [x] **Step 1: Add the `idle` tone to the `StatusTone` union**

Change the union from:

```ts
type StatusTone = "live" | "ready" | "attention" | "fault" | "off";
```

to:

```ts
type StatusTone = "live" | "ready" | "idle" | "attention" | "fault" | "off";
```

- [x] **Step 2: Map Idle to the new tone**

In `META`, change only the Idle entry’s `tone`:

```ts
tone: "idle",
```

Keep `busy: false`, `canSend: true`, the `Idle` label, and its existing hint.

- [x] **Step 3: Map the new tone to the existing subdued utility**

Add the entry to `TONE_TEXT`:

```ts
idle: "text-dim",
```

Do not modify the `live` mapping; it must remain `text-amber-ink` so the running lamp animation and running label use the same color.

- [x] **Step 4: Run the focused tests and verify they pass**

```bash
cd clients/web && npm run test:unit -- src/lib/status.test.ts
```

Expected: PASS.

- [x] **Step 5: Run web typecheck and build**

```bash
cd clients/web && npm run typecheck && npm run build
```

Expected: both commands exit successfully.

- [x] **Step 6: Commit the implementation**

```bash
git add clients/web/src/lib/status.ts

git commit -m "fix: quiet idle session status color"
```

### Task 3: Verify the complete change

**Files:**
- Verify: `clients/web/src/lib/status.test.ts`
- Verify: `clients/web/src/lib/status.ts`

**Interfaces:**
- Consumes: The completed status metadata implementation and its focused tests.
- Produces: Verified, clean branch ready for PR preparation.

- [x] **Step 1: Run all web unit tests**

```bash
cd clients/web && npm run test:unit
```

Expected: PASS with zero failures.

- [x] **Step 2: Check formatting and diff integrity**

```bash
cargo fmt --check
git diff origin/main...HEAD --check
git status --short
```

Expected: formatting passes, diff check reports no whitespace errors, and only the intended spec, test, and status mapping files are committed with no untracked working-tree changes.

- [x] **Step 3: Review the final diff**

```bash
git diff --stat origin/main...HEAD
git log --oneline origin/main..HEAD
```

Confirm the diff contains only the design spec, status metadata test, and semantic Idle color change; confirm Running remains live/amber/busy.
