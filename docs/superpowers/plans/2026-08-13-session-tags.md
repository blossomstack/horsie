# Session Tags Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace single-parent session groups with many-per-session tags, and bring six drifted page headers back onto the app's header contract.

**Architecture:** A tag is an annotation key `tag.<name>` on a session, written through the `PUT /api/sessions/{id}/annotations` route that already exists. The tag universe is derived client-side from the session list, so a tag is created by being used and deleted by being unused — there is no registry, which is why the whole group subsystem (4 routes, 4 supervisor commands, 3 journal events, 5 wire types) is deleted rather than adapted.

**Tech Stack:** Rust (axum, horsie-actor supervisor, fluorite schemas), React 19 + TanStack Query + Tailwind v4, vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-08-13-session-tags-design.md`

## Global Constraints

- Tag names: lowercase `[a-z0-9._-]`, 1–124 characters. Enforced server-side by the existing `valid_annotation_key` (128 chars minus the 4-char `tag.` prefix); normalised client-side before any request.
- Annotation value for a tag is always the empty string. Presence of the key is the tag.
- No backward compatibility. `group=` annotations and `Group*` journal events are deleted, not migrated. An existing deployment clears its supervisor journal state on upgrade.
- Header contract: `flex h-[3.25rem] shrink-0 items-center gap-2 border-b bg-panel px-4 sm:gap-3 sm:px-6`.
- Web work runs under `bun`, never `npm` (`bun install --frozen-lockfile`, `bun run test`, `bunx playwright test`).
- `.fl` edits require `make types` (regenerates `clients/web/src/generated/`); fluorite never deletes orphaned files, so removed types leave stale files that must be deleted by hand.
- Rust: `cargo fmt` before `cargo clippy --all-targets --all-features -- -D warnings`. Production code denies `unwrap_used` / `expect_used` / `panic`.

---

### Task 1: The pure tag library

The whole feature's logic lives here as pure functions over `SessionSummary`, so the components stay dumb and the rules are tested without a DOM.

**Files:**
- Create: `clients/web/src/lib/sessionTags.ts`
- Test: `clients/web/src/lib/sessionTags.test.ts`

**Interfaces:**
- Consumes: `SessionSummary` from `../api/types` (carries `annotations: {key, value}[]`).
- Produces:
  - `TAG_PREFIX: "tag."`
  - `sessionTags(s: SessionSummary): string[]` — sorted
  - `allTags(sessions: SessionSummary[]): string[]` — sorted, deduped
  - `normalizeTagName(raw: string): string | undefined`
  - `TagFilter = { require: string[]; exclude: string[] }`
  - `EMPTY_FILTER: TagFilter`
  - `matchesTagFilter(s: SessionSummary, f: TagFilter): boolean`
  - `cycleTag(f: TagFilter, tag: string): TagFilter`
  - `tagState(f: TagFilter, tag: string): "neutral" | "require" | "exclude"`
  - `filterIsActive(f: TagFilter): boolean`
  - `reconcileFilter(saved: TagFilter, universe: string[]): TagFilter`

- [ ] **Step 1: Write the failing test**

```ts
import { describe, expect, it } from "vitest";
import type { SessionSummary } from "../api/types";
import { SessionStatusKind } from "../api/types";
import {
  allTags,
  cycleTag,
  EMPTY_FILTER,
  filterIsActive,
  matchesTagFilter,
  normalizeTagName,
  reconcileFilter,
  sessionTags,
  tagState,
} from "./sessionTags";

function session(id: string, tags: string[] = [], extra: { key: string; value: string }[] = []): SessionSummary {
  return {
    id,
    name: `session ${id}`,
    status: SessionStatusKind.Idle,
    createdAt: 1,
    annotations: [...tags.map((t) => ({ key: `tag.${t}`, value: "" })), ...extra],
    forks: [],
  };
}

describe("sessionTags", () => {
  it("reads tag.* keys, sorted, ignoring other annotations", () => {
    expect(sessionTags(session("a", ["web", "api"], [{ key: "source", value: "routine" }])))
      .toEqual(["api", "web"]);
  });

  it("treats a bare `tag.` key as no tag", () => {
    expect(sessionTags(session("a", [], [{ key: "tag.", value: "" }]))).toEqual([]);
  });

  it("keeps a dotted tag whole", () => {
    expect(sessionTags(session("a", ["v2.migration"]))).toEqual(["v2.migration"]);
  });
});

describe("allTags", () => {
  it("unions and dedupes across sessions, sorted", () => {
    expect(allTags([session("a", ["web"]), session("b", ["api", "web"])]))
      .toEqual(["api", "web"]);
  });

  it("forgets a tag once its last carrier is gone", () => {
    expect(allTags([session("a")])).toEqual([]);
  });
});

describe("normalizeTagName", () => {
  it("lowercases and hyphenates whitespace", () => {
    expect(normalizeTagName("  Bug   Fix ")).toBe("bug-fix");
  });

  it("strips characters the annotation key charset rejects", () => {
    expect(normalizeTagName("we:b!")).toBe("web");
  });

  it("rejects a name that normalises to nothing", () => {
    expect(normalizeTagName("  !!  ")).toBeUndefined();
  });

  it("rejects a name over 124 characters", () => {
    expect(normalizeTagName("a".repeat(125))).toBeUndefined();
    expect(normalizeTagName("a".repeat(124))).toBe("a".repeat(124));
  });
});

describe("matchesTagFilter", () => {
  const s = session("a", ["web", "done"]);

  it("matches everything when empty", () => {
    expect(matchesTagFilter(s, EMPTY_FILTER)).toBe(true);
    expect(matchesTagFilter(session("b"), EMPTY_FILTER)).toBe(true);
  });

  it("ANDs every required tag", () => {
    expect(matchesTagFilter(s, { require: ["web", "done"], exclude: [] })).toBe(true);
    expect(matchesTagFilter(s, { require: ["web", "api"], exclude: [] })).toBe(false);
  });

  it("rejects a session carrying an excluded tag", () => {
    expect(matchesTagFilter(s, { require: [], exclude: ["done"] })).toBe(false);
    expect(matchesTagFilter(session("b"), { require: [], exclude: ["done"] })).toBe(true);
  });

  it("matches nothing when a tag is both required and excluded", () => {
    expect(matchesTagFilter(s, { require: ["web"], exclude: ["web"] })).toBe(false);
  });
});

describe("cycleTag / tagState", () => {
  it("cycles neutral to require to exclude and back", () => {
    let f = EMPTY_FILTER;
    expect(tagState(f, "web")).toBe("neutral");
    f = cycleTag(f, "web");
    expect(tagState(f, "web")).toBe("require");
    f = cycleTag(f, "web");
    expect(tagState(f, "web")).toBe("exclude");
    f = cycleTag(f, "web");
    expect(tagState(f, "web")).toBe("neutral");
    expect(filterIsActive(f)).toBe(false);
  });

  it("leaves other tags alone", () => {
    const f = cycleTag({ require: ["api"], exclude: [] }, "web");
    expect(f.require).toEqual(["api", "web"]);
  });
});

describe("reconcileFilter", () => {
  it("drops constraints naming a tag nobody carries", () => {
    expect(reconcileFilter({ require: ["web", "gone"], exclude: ["dead"] }, ["web"]))
      .toEqual({ require: ["web"], exclude: [] });
  });
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd clients/web && bun run test src/lib/sessionTags.test.ts`
Expected: FAIL — cannot resolve `./sessionTags`.

- [ ] **Step 3: Implement**

```ts
import type { SessionSummary } from "../api/types";

/** A tag lives in its own annotation namespace, so a future `source=` or
 * `origin=` key can never be mistaken for one. */
export const TAG_PREFIX = "tag.";

/** The annotation key charset the server enforces, minus the prefix budget. */
const MAX_TAG_LEN = 124;

export interface TagFilter {
  require: string[];
  exclude: string[];
}

export const EMPTY_FILTER: TagFilter = { require: [], exclude: [] };

/** This session's tags, sorted. `tag.` with nothing after it is not a tag. */
export function sessionTags(s: SessionSummary): string[] {
  return s.annotations
    .filter((a) => a.key.startsWith(TAG_PREFIX) && a.key.length > TAG_PREFIX.length)
    .map((a) => a.key.slice(TAG_PREFIX.length))
    .sort();
}

/** Every tag in existence. Derived, never stored: this is what makes a tag
 * appear the moment it is used and vanish when its last carrier drops it. */
export function allTags(sessions: SessionSummary[]): string[] {
  const seen = new Set<string>();
  for (const s of sessions) for (const t of sessionTags(s)) seen.add(t);
  return [...seen].sort();
}

/** What the user typed, as a tag the server will accept — or nothing, when
 * there is no tag left after normalising. Rejecting `Bug Fix` would be
 * pedantry; `bug-fix` is what they meant. */
export function normalizeTagName(raw: string): string | undefined {
  const name = raw
    .trim()
    .toLowerCase()
    .replace(/\s+/g, "-")
    .replace(/[^a-z0-9._-]/g, "");
  if (!name || name.length > MAX_TAG_LEN) return undefined;
  return name;
}

export function matchesTagFilter(s: SessionSummary, f: TagFilter): boolean {
  if (!filterIsActive(f)) return true;
  const tags = new Set(sessionTags(s));
  return (
    f.require.every((t) => tags.has(t)) && !f.exclude.some((t) => tags.has(t))
  );
}

export function tagState(f: TagFilter, tag: string): "neutral" | "require" | "exclude" {
  if (f.require.includes(tag)) return "require";
  if (f.exclude.includes(tag)) return "exclude";
  return "neutral";
}

/** neutral → require → exclude → neutral. */
export function cycleTag(f: TagFilter, tag: string): TagFilter {
  switch (tagState(f, tag)) {
    case "neutral":
      return { require: [...f.require, tag], exclude: f.exclude };
    case "require":
      return {
        require: f.require.filter((t) => t !== tag),
        exclude: [...f.exclude, tag],
      };
    case "exclude":
      return { require: f.require, exclude: f.exclude.filter((t) => t !== tag) };
  }
}

export function filterIsActive(f: TagFilter): boolean {
  return f.require.length > 0 || f.exclude.length > 0;
}

/** Drop constraints for tags that no longer exist. A persisted filter naming
 * a tag whose last session was deleted would hide the whole rail with no
 * visible cause. */
export function reconcileFilter(saved: TagFilter, universe: string[]): TagFilter {
  const live = new Set(universe);
  return {
    require: saved.require.filter((t) => live.has(t)),
    exclude: saved.exclude.filter((t) => live.has(t)),
  };
}
```

- [ ] **Step 4: Run the tests**

Run: `cd clients/web && bun run test src/lib/sessionTags.test.ts`
Expected: PASS, all cases.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/lib/sessionTags.ts clients/web/src/lib/sessionTags.test.ts
git commit -m "feat(web): tag derivation, normalisation, and tri-state filtering"
```

---

### Task 2: The tag mutation hook

**Files:**
- Create: `clients/web/src/hooks/useSessionTags.ts`
- Test: `clients/web/src/hooks/useSessionTags.test.tsx`
- Delete: `clients/web/src/hooks/useGroups.ts`, `clients/web/src/hooks/useGroups.test.tsx`

**Interfaces:**
- Consumes: `api.sessions.setAnnotations`, `qk` from `./useSessions`.
- Produces: `useSetSessionTag(): UseMutationResult` taking `{ id: string; tag: string; on: boolean }`.

- [ ] **Step 1: Write the failing test**

```tsx
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import { useSetSessionTag } from "./useSessionTags";

vi.mock("../api/client", () => ({
  api: { sessions: { setAnnotations: vi.fn() } },
}));

function wrapper(client: QueryClient) {
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

afterEach(() => vi.clearAllMocks());

describe("useSetSessionTag", () => {
  it("sets an empty-valued tag key when turning a tag on", async () => {
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    const client = new QueryClient();
    const { result } = renderHook(() => useSetSessionTag(), { wrapper: wrapper(client) });
    await result.current.mutateAsync({ id: "s1", tag: "web", on: true });
    expect(api.sessions.setAnnotations).toHaveBeenCalledWith("s1", {
      set: [{ key: "tag.web", value: "" }],
      remove: [],
    });
  });

  it("removes the key when turning a tag off, and invalidates both queries", async () => {
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    const client = new QueryClient();
    const spy = vi.spyOn(client, "invalidateQueries");
    const { result } = renderHook(() => useSetSessionTag(), { wrapper: wrapper(client) });
    await result.current.mutateAsync({ id: "s1", tag: "web", on: false });
    expect(api.sessions.setAnnotations).toHaveBeenCalledWith("s1", {
      set: [],
      remove: ["tag.web"],
    });
    expect(spy).toHaveBeenCalledWith({ queryKey: ["sessions"] });
    expect(spy).toHaveBeenCalledWith({ queryKey: ["session", "s1"] });
  });
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd clients/web && bun run test src/hooks/useSessionTags.test.tsx`
Expected: FAIL — cannot resolve `./useSessionTags`.

- [ ] **Step 3: Implement**

```ts
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import { TAG_PREFIX } from "../lib/sessionTags";
import { qk } from "./useSessions";

/** Assign or unassign one tag. Both directions are the same annotation
 * merge-update, which is why tags need no endpoint of their own. */
export function useSetSessionTag() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ id, tag, on }: { id: string; tag: string; on: boolean }) =>
      api.sessions.setAnnotations(id, {
        set: on ? [{ key: `${TAG_PREFIX}${tag}`, value: "" }] : [],
        remove: on ? [] : [`${TAG_PREFIX}${tag}`],
      }),
    onSuccess: (_r, { id }) => {
      client.invalidateQueries({ queryKey: qk.sessions });
      client.invalidateQueries({ queryKey: qk.session(id) });
    },
  });
}
```

- [ ] **Step 4: Run the tests**

Run: `cd clients/web && bun run test src/hooks/useSessionTags.test.tsx`
Expected: PASS.

- [ ] **Step 5: Delete the group hook and commit**

```bash
git rm clients/web/src/hooks/useGroups.ts clients/web/src/hooks/useGroups.test.tsx
git add clients/web/src/hooks/useSessionTags.ts clients/web/src/hooks/useSessionTags.test.tsx
git commit -m "feat(web): tag assignment hook, replacing the group hooks"
```

Note: this leaves `Sidebar.tsx`, `SessionRow.tsx`, and `SessionGroupSection.tsx` importing a deleted module. Tasks 3 and 4 close that; do not run a full typecheck until Task 4 lands.

---

### Task 3: The session row's tag menu

**Files:**
- Modify: `clients/web/src/components/SessionRow.tsx`
- Test: `clients/web/src/components/SessionRow.test.tsx` (create)

**Interfaces:**
- Consumes: `useSetSessionTag` (Task 2), `sessionTags` / `normalizeTagName` (Task 1), `Menu` / `MenuItem` from `./Menu`.
- Produces: `SessionRow({ s, tags }: { s: SessionSummary; tags: string[] })` — `tags` is the full universe from the rail. `GROUP_DRAG_MIME` and `SESSION_DRAG_MIME` are both removed.

`MenuItem` closes the menu on select, which is wrong for a checklist where two tags are a normal edit. Add an optional `keepOpen` prop to `MenuItem` rather than building a second menu primitive.

- [ ] **Step 1: Write the failing test**

```tsx
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import type { SessionSummary } from "../api/types";
import { SessionStatusKind } from "../api/types";
import { SessionRow } from "./SessionRow";

vi.mock("../api/client", () => ({
  api: { sessions: { setAnnotations: vi.fn(), remove: vi.fn() } },
}));

afterEach(cleanup);

function row(s: SessionSummary, tags: string[]) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <SessionRow s={s} tags={tags} />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

const tagged: SessionSummary = {
  id: "s1",
  name: "one",
  status: SessionStatusKind.Idle,
  createdAt: 1,
  annotations: [{ key: "tag.web", value: "" }],
  forks: [],
};

describe("SessionRow tag menu", () => {
  it("unassigns a tag the session carries", async () => {
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    row(tagged, ["web", "api"]);
    fireEvent.click(screen.getByTestId("session-row-menu-s1"));
    fireEvent.click(screen.getByTestId("toggle-tag-web"));
    await waitFor(() =>
      expect(api.sessions.setAnnotations).toHaveBeenCalledWith("s1", {
        set: [],
        remove: ["tag.web"],
      }),
    );
  });

  it("assigns a tag the session lacks", async () => {
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    row(tagged, ["web", "api"]);
    fireEvent.click(screen.getByTestId("session-row-menu-s1"));
    fireEvent.click(screen.getByTestId("toggle-tag-api"));
    await waitFor(() =>
      expect(api.sessions.setAnnotations).toHaveBeenCalledWith("s1", {
        set: [{ key: "tag.api", value: "" }],
        remove: [],
      }),
    );
  });

  it("creates a tag from the input, normalising what was typed", async () => {
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    row(tagged, ["web"]);
    fireEvent.click(screen.getByTestId("session-row-menu-s1"));
    const input = screen.getByTestId("new-tag-input");
    fireEvent.change(input, { target: { value: "Bug Fix" } });
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() =>
      expect(api.sessions.setAnnotations).toHaveBeenCalledWith("s1", {
        set: [{ key: "tag.bug-fix", value: "" }],
        remove: [],
      }),
    );
  });

  it("sends nothing for a name that normalises to nothing", () => {
    row(tagged, []);
    fireEvent.click(screen.getByTestId("session-row-menu-s1"));
    const input = screen.getByTestId("new-tag-input");
    fireEvent.change(input, { target: { value: "  !!  " } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(api.sessions.setAnnotations).not.toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd clients/web && bun run test src/components/SessionRow.test.tsx`
Expected: FAIL — `toggle-tag-web` not found (the row still renders group items).

- [ ] **Step 3: Add `keepOpen` to `MenuItem`**

In `clients/web/src/components/Menu.tsx`, add the prop and skip the close:

```tsx
export function MenuItem({
  onSelect,
  danger,
  testId,
  /** Leave the menu open after selecting. For a checklist, where editing two
   * entries is one edit, not two trips. */
  keepOpen,
  children,
}: {
  onSelect: () => void;
  danger?: boolean;
  testId?: string;
  keepOpen?: boolean;
  children: ReactNode;
}) {
  const close = useContext(CloseContext);
  return (
    <button
      type="button"
      role="menuitem"
      data-testid={testId}
      className={cn(
        "block w-full px-3 py-1.5 text-left text-[13px] transition-colors hover:bg-raised",
        danger ? "text-red-ink" : "text-legend",
      )}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        if (!keepOpen) close();
        onSelect();
      }}
    >
      {children}
    </button>
  );
}
```

- [ ] **Step 4: Rewrite the row's menu**

In `SessionRow.tsx`: drop the `GROUP_DRAG_MIME` / `SESSION_DRAG_MIME` exports and the `draggable` / `onDragStart` props on the `NavLink` (nothing accepts a drop any more), swap the `groups` prop for `tags`, replace `useSetSessionAnnotations` with `useSetSessionTag`, and render:

```tsx
const mine = new Set(sessionTags(s));
const setTag = useSetSessionTag();
const [draft, setDraft] = useState("");

const submitTag = () => {
  const name = normalizeTagName(draft);
  if (!name) return;
  setTag.mutate({ id: s.id, tag: name, on: true });
  setDraft("");
};
```

```tsx
<Menu label="Session actions" testId={`session-row-menu-${s.id}`}>
  {tags.map((t) => (
    <MenuItem
      key={t}
      keepOpen
      testId={`toggle-tag-${t}`}
      onSelect={() => setTag.mutate({ id: s.id, tag: t, on: !mine.has(t) })}
    >
      <span className="flex items-center gap-2">
        <Check
          size={12}
          aria-hidden
          className={cn("shrink-0", mine.has(t) ? "opacity-100" : "opacity-0")}
        />
        <span className="min-w-0 truncate">{t}</span>
      </span>
    </MenuItem>
  ))}
  {/* The only way a tag comes into existence. */}
  <div className="px-2 py-1.5">
    <input
      data-testid="new-tag-input"
      className="w-full rounded-[var(--radius-control)] border bg-panel px-2 py-1 text-[0.8125rem] text-legend outline-none placeholder:text-faint focus:border-[var(--rule-strong)]"
      placeholder="New tag…"
      value={draft}
      onChange={(e) => setDraft(e.target.value)}
      onKeyDown={(e) => {
        if (e.key === "Enter") submitTag();
      }}
    />
  </div>
  <div className="my-1 border-t" role="separator" />
  <MenuItem danger testId={`delete-session-${s.id}`} onSelect={() => void remove()}>
    Delete session
  </MenuItem>
</Menu>
```

- [ ] **Step 5: Run the tests**

Run: `cd clients/web && bun run test src/components/SessionRow.test.tsx src/components/Menu.test.tsx`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add clients/web/src/components/SessionRow.tsx clients/web/src/components/SessionRow.test.tsx clients/web/src/components/Menu.tsx
git commit -m "feat(web): tag checklist in the session row menu"
```

---

### Task 4: The flat rail and the filter panel

**Files:**
- Modify: `clients/web/src/components/Sidebar.tsx`
- Create: `clients/web/src/components/TagFilterPanel.tsx`
- Modify: `clients/web/src/components/Sidebar.test.tsx`
- Delete: `clients/web/src/components/SessionGroupSection.tsx`, `clients/web/src/lib/sessionGroups.ts`, `clients/web/src/lib/sessionGroups.test.ts`

**Interfaces:**
- Consumes: `allTags` / `matchesTagFilter` / `cycleTag` / `tagState` / `filterIsActive` / `reconcileFilter` / `EMPTY_FILTER` (Task 1), `SessionRow` with the `tags` prop (Task 3), `usePersistentState`.
- Produces: `TagFilterPanel({ tags, filter, onChange })`.

- [ ] **Step 1: Write the failing Sidebar tests**

Replace the group-oriented cases in `Sidebar.test.tsx`. The mock loses `sessionGroups` entirely; the `session()` helper takes tags:

```tsx
function session(id: string, tags: string[] = []): SessionSummary {
  return {
    id,
    name: `session ${id}`,
    status: SessionStatusKind.Idle,
    createdAt: 1,
    annotations: tags.map((t) => ({ key: `tag.${t}`, value: "" })),
    forks: [],
  };
}

describe("Sidebar tag filter", () => {
  it("hides the filter button until a tag exists", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [session("a")] });
    renderSidebar();
    await screen.findByTestId("session-row");
    expect(screen.queryByTestId("tag-filter-button")).toBeNull();
  });

  it("cycles a chip through require and exclude, narrowing the list each way", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("a", ["web"]), session("b")],
    });
    renderSidebar();
    await waitFor(() => expect(screen.getAllByTestId("session-row")).toHaveLength(2));

    fireEvent.click(screen.getByTestId("tag-filter-button"));
    const chip = screen.getByTestId("tag-chip-web");

    fireEvent.click(chip);
    expect(chip).toHaveAttribute("data-state", "require");
    await waitFor(() => expect(screen.getAllByTestId("session-row")).toHaveLength(1));
    expect(screen.getByTestId("session-row")).toHaveAttribute("data-session-id", "a");

    fireEvent.click(chip);
    expect(chip).toHaveAttribute("data-state", "exclude");
    await waitFor(() => expect(screen.getAllByTestId("session-row")).toHaveLength(1));
    expect(screen.getByTestId("session-row")).toHaveAttribute("data-session-id", "b");

    fireEvent.click(screen.getByTestId("clear-tag-filter"));
    await waitFor(() => expect(screen.getAllByTestId("session-row")).toHaveLength(2));
  });

  it("says so when the tag filter empties the list", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [session("a", ["web"])] });
    renderSidebar();
    await screen.findByTestId("session-row");
    fireEvent.click(screen.getByTestId("tag-filter-button"));
    fireEvent.click(screen.getByTestId("tag-chip-web"));
    fireEvent.click(screen.getByTestId("tag-chip-web"));
    expect(await screen.findByTestId("no-tag-matches")).toBeInTheDocument();
  });
});
```

Also delete every remaining test in the file that names a group section, drag-and-drop, collapse, or `new-group-button`, and drop `localStorage` keys `horsie.session-group-order` / `horsie.session-groups-collapsed` from any setup.

- [ ] **Step 2: Run it and watch it fail**

Run: `cd clients/web && bun run test src/components/Sidebar.test.tsx`
Expected: FAIL — `tag-filter-button` not found.

- [ ] **Step 3: Write `TagFilterPanel`**

```tsx
import { Check, Minus } from "lucide-react";
import { cn } from "../lib/cn";
import { cycleTag, filterIsActive, tagState, type TagFilter } from "../lib/sessionTags";

/** The tag chips, between the Sessions title and the list. Three states per
 * chip, because "show me web" and "hide anything done" are both filters and
 * only one of them is expressible with a checkbox. */
export function TagFilterPanel({
  tags,
  filter,
  onChange,
}: {
  tags: string[];
  filter: TagFilter;
  onChange: (next: TagFilter) => void;
}) {
  return (
    <div className="flex flex-wrap items-center gap-1 px-2 pb-2" data-testid="tag-filter-panel">
      {tags.map((t) => {
        const state = tagState(filter, t);
        return (
          <button
            key={t}
            type="button"
            data-testid={`tag-chip-${t}`}
            data-state={state}
            aria-label={
              state === "require" ? `${t} — required`
              : state === "exclude" ? `${t} — excluded`
              : t
            }
            className={cn(
              "chip transition-colors",
              state === "require" && "!border-[var(--rule-strong)] !bg-raised !text-legend",
              state === "exclude" && "!text-faint line-through",
            )}
            onClick={() => onChange(cycleTag(filter, t))}
          >
            {state === "require" && <Check size={10} aria-hidden />}
            {state === "exclude" && <Minus size={10} aria-hidden />}
            {t}
          </button>
        );
      })}
      {filterIsActive(filter) && (
        <button
          type="button"
          data-testid="clear-tag-filter"
          className="legend px-1.5 py-0.5 hover:!text-legend"
          onClick={() => onChange({ require: [], exclude: [] })}
        >
          Clear
        </button>
      )}
    </div>
  );
}
```

- [ ] **Step 4: Flatten the Sidebar**

Remove: the `useGroupList` / `useCreateGroup` imports and calls, `addingGroup` / `newGroupName` state and its inline form, both `usePersistentState` group keys, `unionGroups` / `reconcileOrder` / `partitionSessions` use, the `SessionGroupSection` import, and the `new-group-button`. Add:

```tsx
const [savedFilter, setSavedFilter] = usePersistentState<TagFilter>(
  "horsie.session-tag-filter",
  EMPTY_FILTER,
  {
    deserialize: (raw) => {
      if (typeof raw !== "object" || raw === null) return undefined;
      const { require, exclude } = raw as Partial<TagFilter>;
      const ok = (v: unknown) => Array.isArray(v) && v.every((x) => typeof x === "string");
      return ok(require) && ok(exclude)
        ? { require: require as string[], exclude: exclude as string[] }
        : undefined;
    },
  },
);
const [panelOpen, setPanelOpen] = useState(false);

const tags = useMemo(() => allTags(sessions ?? []), [sessions]);
// A constraint naming a tag nobody carries any more would hide the rail with
// no visible cause, so the live universe is what the filter is read through.
const filter = useMemo(() => reconcileFilter(savedFilter, tags), [savedFilter, tags]);

const needle = filter_text.trim().toLowerCase();
const shown = useMemo(
  () =>
    (sessions ?? [])
      .filter((s) => matchesTagFilter(s, filter))
      .filter(
        (s) =>
          !needle ||
          [sessionTitle(s.name), s.workflow ?? ""].join(" ").toLowerCase().includes(needle),
      ),
  [sessions, filter, needle],
);
```

Rename the existing text-filter state to `filterText` so it does not collide with `filter`. The title row becomes:

```tsx
<div className="flex items-center justify-between pb-1.5 pl-4 pr-2 pt-4">
  <span className="legend">Sessions</span>
  <div className="flex items-center gap-0.5">
    {tags.length > 0 && (
      <button
        className={cn(
          "key-icon !h-6 !w-6",
          filterIsActive(filter) && "!bg-raised !text-legend shadow-[inset_0_0_0_1px_var(--rule-strong)]",
        )}
        onClick={() => setPanelOpen((v) => !v)}
        aria-expanded={panelOpen}
        data-testid="tag-filter-button"
        title="Filter by tag"
        aria-label="Filter by tag"
      >
        <ListFilter size={14} aria-hidden />
      </button>
    )}
    <button className="key-icon !h-6 !w-6" onClick={() => navigate("/")} data-testid="new-session-button" title="Start a new session" aria-label="Start a new session">
      <Plus size={14} aria-hidden />
    </button>
  </div>
</div>
{panelOpen && tags.length > 0 && (
  <TagFilterPanel tags={tags} filter={filter} onChange={setSavedFilter} />
)}
```

The list itself becomes flat — rows and their forks, no sections:

```tsx
{!isLoading && !isError && shown.map((s) => (
  <div key={s.id}>
    <SessionRow s={s} tags={tags} />
    {forkTree(s.forks).map(({ fork, depth }) => (
      <ForkRow key={fork.id} sessionId={s.id} fork={fork} depth={depth} />
    ))}
  </div>
))}
```

Empty states have to name the filter that emptied the list, or a filtered rail reads as a lost account:

```tsx
{!isLoading && !isError && shown.length === 0 && (sessions?.length ?? 0) > 0 && (
  needle !== "" ? (
    <p className="px-2.5 py-8 text-[0.8125rem] leading-relaxed text-faint" data-testid="no-text-matches">
      No session matches “{filterText.trim()}”.
    </p>
  ) : (
    <p className="px-2.5 py-8 text-[0.8125rem] leading-relaxed text-faint" data-testid="no-tag-matches">
      No session matches these tags.
    </p>
  )
)}
```

- [ ] **Step 5: Run the tests**

Run: `cd clients/web && bun run test src/components/Sidebar.test.tsx && bunx tsc --noEmit -p tsconfig.app.json`
Expected: PASS, and a clean typecheck now that nothing imports the deleted modules.

- [ ] **Step 6: Delete the group UI and commit**

```bash
git rm clients/web/src/components/SessionGroupSection.tsx clients/web/src/lib/sessionGroups.ts clients/web/src/lib/sessionGroups.test.ts
git add clients/web/src/components/Sidebar.tsx clients/web/src/components/Sidebar.test.tsx clients/web/src/components/TagFilterPanel.tsx
git commit -m "feat(web): flat session rail with a tri-state tag filter"
```

---

### Task 5: Delete the group API from the client and the schema

**Files:**
- Modify: `clients/web/src/api/client.ts`, `crates/models/fluorite/session_api.fl`
- Delete: `clients/web/src/generated/session_api/{sessionGroupView,createGroupRequest,createGroupResponse,renameGroupRequest,listGroupsResponse}.ts`

- [ ] **Step 1: Drop the schema types**

Remove `SessionGroupView`, `CreateGroupRequest`, `CreateGroupResponse`, `RenameGroupRequest`, and `ListGroupsResponse` from `session_api.fl`. Keep `RenameSessionRequest`, which sits between them.

- [ ] **Step 2: Regenerate and clean up orphans**

```bash
make types
git status --short clients/web/src/generated
```

fluorite never deletes what it no longer generates, so the five removed types leave stale files behind. Delete them and confirm `clients/web/src/generated/session_api/index.ts` no longer re-exports them.

- [ ] **Step 3: Drop the `sessionGroups` client**

Remove the whole `sessionGroups` block from `clients/web/src/api/client.ts` and the now-unused type imports (`ListGroupsResponse`, `CreateGroupRequest`, `CreateGroupResponse`, `RenameGroupRequest`).

- [ ] **Step 4: Verify**

Run: `cd clients/web && bunx tsc --noEmit -p tsconfig.app.json && bun run test`
Expected: clean typecheck, full web suite green.

- [ ] **Step 5: Commit**

```bash
git add -A clients/web crates/models/fluorite/session_api.fl
git commit -m "refactor: drop the session-group wire types and client"
```

---

### Task 6: Delete the group registry from the server

**Files:**
- Rename: `crates/server/src/http/groups.rs` → `crates/server/src/http/annotations.rs`
- Modify: `crates/server/src/http/mod.rs`, `crates/server/src/sessions/supervisor.rs`

- [ ] **Step 1: Reduce the handler module**

`git mv crates/server/src/http/groups.rs crates/server/src/http/annotations.rs`, then delete `list_groups`, `create_group`, `rename_group`, `delete_group`, and `group_error` from it, leaving `valid_annotation_key` and `set_annotations`. Fix the module doc comment — it currently claims to be about groups.

- [ ] **Step 2: Reduce the routes**

In `http/mod.rs`: `mod groups;` → `mod annotations;`, the `set_annotations` route now points at `annotations::set_annotations`, and both `/api/session-groups` routes go. Delete the `group_crud_round_trip` test and the group half of `annotations_ride_the_session_list_and_follow_group_edits` — what survives is the annotation round-trip, renamed accordingly.

- [ ] **Step 3: Reduce the supervisor**

From `sessions/supervisor.rs` delete: the `CreateGroup` / `RenameGroup` / `DeleteGroup` / `ListGroups` command variants and their `handle` arms; the `GroupCreated` / `GroupRenamed` / `GroupDeleted` event variants and their fold arms; `GroupRecord`; `GroupError` with its `Display` / `Error` impls; `validate_group_name`; `group_exists`; `GROUP_NAME_MAX_LEN`; and the `groups` field on `SessionSupervisorState`. Delete the tests `group_rename_rewrites_annotations`, `group_delete_strips_annotations`, and `rename_unregistered_group_rewrites_annotations`. Keep `annotations_set_and_removed_fold` and `session_delete_drops_its_annotations`, adjusting their fixtures to use `tag.` keys.

- [ ] **Step 4: Record the journal failure mode**

Before committing, establish what an existing journal holding a `GroupCreated` event does now — it decides what the PR body tells the operator. Write a throwaway test that folds a JSON event payload of the removed shape through the supervisor's event decoder and observe whether it errors or is skipped. Note the answer in the commit message and delete the probe.

- [ ] **Step 5: Verify**

```bash
cargo fmt
cargo clippy -p horsie-server --all-targets --all-features -- -D warnings
cargo test -p horsie-server --lib
```
Expected: clean, and no reference to `group` survives `rg -i 'group' crates/server/src/sessions/supervisor.rs crates/server/src/http/`.

- [ ] **Step 6: Commit**

```bash
git add -A crates/server
git commit -m "refactor(server): delete the session-group registry"
```

---

### Task 7: Header alignment and the home link

**Files:**
- Modify: `pages/agents/AgentsPage.tsx`, `pages/environments/EnvironmentsPage.tsx`, `pages/workflows/WorkflowsPage.tsx`, `pages/workflows/WorkflowDetailPage.tsx`, `pages/workflows/WorkflowEditPage.tsx`, `pages/routines/RoutinesPage.tsx`, `pages/routines/RoutineDetailPage.tsx`, `pages/routines/RoutineEditPage.tsx`, `components/Sidebar.tsx` (all under `clients/web/src/`)

- [ ] **Step 1: Align the eight headers**

Each header `div` becomes:

```tsx
<div className="flex h-[3.25rem] shrink-0 items-center gap-2 border-b bg-panel px-4 sm:gap-3 sm:px-6">
```

Delete the subtitle `<p>` from `AgentsPage` and `EnvironmentsPage`, and the now-redundant `<div className="min-w-0 flex-1">` wrapper around their `<h1>` — the title takes `min-w-0 flex-1 truncate` itself. Where a header's action button relied on `ml-auto`, keep it.

- [ ] **Step 2: Link the nameplate**

In `Sidebar.tsx`, wrap the `h` chip and the wordmark in a link, leaving the offline lamp outside it:

```tsx
<Link
  to="/"
  data-testid="home-link"
  className="flex min-w-0 items-center gap-2.5 rounded-[var(--radius-control)] px-1 py-0.5 -mx-1 transition-colors hover:bg-raised"
>
  <span aria-hidden className="flex h-6 w-6 items-center justify-center rounded-[4px] bg-orange font-mono text-[0.8125rem] font-bold text-orange-ink shadow-[var(--cap-lift)]">
    h
  </span>
  <span className="font-mono text-[0.8125rem] font-semibold tracking-[0.16em] text-legend">
    HORSIE
  </span>
</Link>
```

- [ ] **Step 3: Verify**

```bash
cd clients/web && bunx tsc --noEmit -p tsconfig.app.json && bun run test
```
Expected: green. `AgentsPage.test.tsx` and `EnvironmentsPage.test.tsx` may assert on the deleted subtitle text — if so, drop those assertions.

- [ ] **Step 4: Commit**

```bash
git add -A clients/web/src
git commit -m "fix(web): align page headers and link the nameplate home"
```

---

### Task 8: End-to-end

**Files:**
- Delete: `clients/web/e2e/s-session-groups.spec.ts`
- Create: `clients/web/e2e/s-session-tags.spec.ts`

- [ ] **Step 1: Write the spec**

```ts
// Group S — session tags: create by using, filter three ways, delete by
// disuse, end to end against the real server.

import { expect, test } from "./fixtures";
import { createSession, sendMessage } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("S1: a tag is created by use, filters both ways, and vanishes when dropped", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("hello");
  await createSession(page, appBase);
  const id = await sendMessage(page, "hi");
  const row = page.locator(`[data-testid="session-row"][data-session-id="${id}"]`);

  // No tags anywhere yet, so nothing to filter by.
  await expect(page.getByTestId("tag-filter-button")).toHaveCount(0);

  // Creating a tag is assigning one that does not exist.
  await row.hover();
  await page.getByTestId(`session-row-menu-${id}`).click();
  await page.getByTestId("new-tag-input").fill("Web UI");
  await page.getByTestId("new-tag-input").press("Enter");
  await page.keyboard.press("Escape");

  // It now exists, so the filter appears and holds it.
  await page.getByTestId("tag-filter-button").click();
  const chip = page.getByTestId("tag-chip-web-ui");
  await expect(chip).toBeVisible();

  // Require it: the tagged session stays.
  await chip.click();
  await expect(chip).toHaveAttribute("data-state", "require");
  await expect(row).toBeVisible();

  // Exclude it: the tagged session goes.
  await chip.click();
  await expect(chip).toHaveAttribute("data-state", "exclude");
  await expect(row).toBeHidden();

  await page.getByTestId("clear-tag-filter").click();
  await expect(row).toBeVisible();

  // Unassign the only carrier and the tag itself is gone.
  await row.hover();
  await page.getByTestId(`session-row-menu-${id}`).click();
  await page.getByTestId("toggle-tag-web-ui").click();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("tag-filter-button")).toHaveCount(0);
});
```

- [ ] **Step 2: Run it**

```bash
cd clients/web && bun run build
TMPDIR=/tmp HORSIE_E2E_SKIP_BUILD=1 bunx playwright test s-session-tags
```
`TMPDIR=/tmp` is required on macOS — the default `$TMPDIR` overruns `sun_path` and kills Playwright setup with no useful error. The build is required because the server serves `dist`, not the dev server.
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git rm clients/web/e2e/s-session-groups.spec.ts
git add clients/web/e2e/s-session-tags.spec.ts
git commit -m "test(e2e): session tags replace session groups"
```

---

### Task 9: Collapse an over-long user message

A pasted log or a long brief currently renders in full, so one message can own
the whole viewport and push the reply the user came back for off screen. It
clamps instead, with a control at its bottom right.

**Files:**
- Create: `clients/web/src/components/CollapsibleText.tsx`
- Create: `clients/web/src/components/CollapsibleText.test.tsx`
- Modify: `clients/web/src/components/Transcript.tsx:155-182` (`UserTurn`)

**Interfaces:**
- Produces: `CollapsibleText({ children, maxHeight }: { children: ReactNode; maxHeight: number })` — `maxHeight` in px, default 320.

Only clamps when the content actually overflows: measure `scrollHeight` against
`maxHeight` in a layout effect, and render no control at all when it fits. A
"More" button under a three-line message is chrome advertising a job it does
not have.

- [ ] **Step 1: Write the failing test**

```tsx
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { CollapsibleText } from "./CollapsibleText";

afterEach(cleanup);

/** jsdom reports every scrollHeight as 0, so overflow is staged explicitly. */
function stageScrollHeight(px: number) {
  vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockReturnValue(px);
}

describe("CollapsibleText", () => {
  it("renders no control when the content fits", () => {
    stageScrollHeight(100);
    render(<CollapsibleText maxHeight={320}>short</CollapsibleText>);
    expect(screen.queryByTestId("expand-text")).toBeNull();
  });

  it("clamps and offers More when the content overflows", () => {
    stageScrollHeight(900);
    render(<CollapsibleText maxHeight={320}>long</CollapsibleText>);
    const body = screen.getByTestId("collapsible-body");
    expect(body).toHaveStyle({ maxHeight: "320px" });
    expect(screen.getByTestId("expand-text")).toHaveTextContent("More");
  });

  it("expands and collapses again", () => {
    stageScrollHeight(900);
    render(<CollapsibleText maxHeight={320}>long</CollapsibleText>);
    fireEvent.click(screen.getByTestId("expand-text"));
    expect(screen.getByTestId("collapsible-body")).not.toHaveStyle({ maxHeight: "320px" });
    expect(screen.getByTestId("expand-text")).toHaveTextContent("Less");
    fireEvent.click(screen.getByTestId("expand-text"));
    expect(screen.getByTestId("expand-text")).toHaveTextContent("More");
  });
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd clients/web && bun run test src/components/CollapsibleText.test.tsx`
Expected: FAIL — cannot resolve `./CollapsibleText`.

- [ ] **Step 3: Implement**

```tsx
import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { cn } from "../lib/cn";

/** Content that clamps to `maxHeight` and offers to open, but only once it
 * actually overflows. A pasted log is otherwise a message that owns the whole
 * viewport and pushes the reply you came back for off screen. */
export function CollapsibleText({
  children,
  maxHeight = 320,
  className,
}: {
  children: ReactNode;
  maxHeight?: number;
  className?: string;
}) {
  const body = useRef<HTMLDivElement>(null);
  const [overflows, setOverflows] = useState(false);
  const [open, setOpen] = useState(false);

  // Measured, not guessed from character count: the same text is two lines
  // wide and twelve narrow, and the rail can be opened or closed under it.
  useLayoutEffect(() => {
    const el = body.current;
    if (!el) return;
    const measure = () => setOverflows(el.scrollHeight > maxHeight + 8);
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [maxHeight, children]);

  const clamped = overflows && !open;

  return (
    <div className="relative">
      <div
        ref={body}
        data-testid="collapsible-body"
        className={cn("overflow-hidden", className)}
        style={clamped ? { maxHeight } : undefined}
      >
        {children}
      </div>
      {clamped && (
        // The fade says the text continues; the button says what to do about
        // it. Without the fade a clamp reads as a message that ends mid-word.
        <div
          aria-hidden
          className="pointer-events-none absolute inset-x-0 bottom-0 h-12 bg-[linear-gradient(to_top,var(--panel-raised),transparent)]"
        />
      )}
      {overflows && (
        <div className="flex justify-end">
          <button
            type="button"
            data-testid="expand-text"
            className="legend relative px-2 py-1 hover:!text-legend"
            aria-expanded={open}
            onClick={() => setOpen((v) => !v)}
          >
            {open ? "Less" : "More"}
          </button>
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 4: Use it in `UserTurn`**

Wrap the existing bubble, leaving its styling intact so nothing else about a
user message changes:

```tsx
<CollapsibleText
  className="rounded-[var(--radius-control)] bg-raised px-3.5 py-2.5 shadow-[inset_0_0_0_1px_var(--row-ring)] text-[0.9375rem] leading-relaxed break-words whitespace-pre-wrap text-legend"
>
  {msg.text}
</CollapsibleText>
```

- [ ] **Step 5: Run the tests**

Run: `cd clients/web && bun run test src/components/CollapsibleText.test.tsx src/components/Transcript.test.tsx`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add clients/web/src/components/CollapsibleText.tsx clients/web/src/components/CollapsibleText.test.tsx clients/web/src/components/Transcript.tsx
git commit -m "feat(web): clamp an over-long user message behind More"
```

---

### Task 10: See it, then ship it

- [ ] **Step 1: Screenshot the real stack**

Write a throwaway `clients/web/e2e/zz-shots.spec.ts` (sorts last, so it never contaminates the FIFO mock queue) that captures, in both themes via `page.emulateMedia({ colorScheme })`: the rail with the tag panel open, the rail with it closed and a filter active, the row's tag menu, a clamped over-long user message with its More control, and the Agents / Environments / Workflows / Routines headers. Run with `TMPDIR=/tmp HORSIE_E2E_SKIP_BUILD=1 bunx playwright test zz-shots`, read every PNG, and **delete the spec before committing**.

- [ ] **Step 2: Fix what the screenshots show**

Header heights are the point of half this change — measure them rather than eyeballing. Anything that looks wrong gets fixed and re-shot.

- [ ] **Step 3: Full local gate**

```bash
cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings
cd clients/web && bunx tsc --noEmit -p tsconfig.app.json && bun run test
```

- [ ] **Step 4: Docs**

`rg -i 'session group' docs/` — nothing today, but confirm before ticking the PR's docs box.

- [ ] **Step 5: Push and open the PR**

Branch `session-tags`, PR body following `.github/pull_request_template.md`: Why / What / Verification / Docs, one long line per paragraph, no hard wrapping. The Verification section states the journal failure mode found in Task 6 and that an existing deployment must clear supervisor state. Do not enable auto-merge.

---

## Self-review

**Spec coverage.** Data model → Task 1 + 2. Deletions → Tasks 2, 4, 5, 6. Flat rail, filter button, tag panel, tri-state, persistence, empty states → Task 4. Row menu → Task 3. Header alignment + nameplate → Task 7. Long-message clamping → Task 9 (added after the spec was written, at the user's request mid-implementation). Testing → Tasks 1–4, 8, 10. Journal consequence → Task 6 step 4, surfaced in the PR body at Task 10.

**Ordering.** The frontend swaps to the annotations route (Tasks 1–4) before the group API is deleted (Tasks 5–6), so no task leaves the server broken. Task 2 does leave the *web* build broken until Task 4 lands, which is called out in Task 2 rather than left to be discovered.

**Type consistency.** `TagFilter` / `EMPTY_FILTER` / `tagState` / `cycleTag` names are used identically in Tasks 1, 4. `useSetSessionTag({id, tag, on})` is defined in Task 2 and called with that exact shape in Task 3. `SessionRow`'s prop is `tags` (not `groups`) in both Task 3 and Task 4. Test ids `tag-filter-button`, `tag-chip-<name>`, `clear-tag-filter`, `toggle-tag-<name>`, `new-tag-input`, `no-tag-matches` are consistent across Tasks 3, 4, 8.
