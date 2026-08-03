# Web UI improvements — design

**Date:** 2026-08-03
**Scope:** `clients/web` — 20 agreed changes across the session view, new-session, agents, settings and admin surfaces, plus three alternate visual worlds.
**Delivery:** one PR, by the user's explicit choice after the size was flagged.

## Why

Twenty items, gathered from using the product. They fall into four groups: chrome that costs space without carrying information, two state bugs that surface after a session is offloaded, several settings surfaces that drifted out of alignment with each other, and one standing complaint — that the interface reads as robotic and its markdown is hard to read.

The first three groups are refinements of the incumbent "instrument console" world (PR #152) and inherit it unchanged. The fourth introduces three alternate visual worlds the user can switch between.

## Decisions taken before design

Recorded because they shaped everything below.

| Question | Choice |
|---|---|
| Delivery | One PR for all 20 items |
| Skin depth | Full skin — materials, borders, radii, shadows, typography |
| Models settings save | Per-item immediate save |
| Composer while running | Stop only; Enter still queues |
| Skins to ship | Paper, Soft, Slate, with Console remaining the default |
| Message copy controls | Two separate icon buttons |
| Config bar state | All icon-only, dot indicator when set |
| Task panel default | Remembered per browser, default closed |

---

## Group A — Session view

### 1. Message chrome

`Gutter` in `Transcript.tsx` is deleted: both the "You"/"Agent" channel word and the timestamp. Turns reclaim its 4.75rem. Role remains legible without labels because the two already differ materially — a user turn is a bordered raised bubble, an agent turn is bare prose.

A hover row appears at the turn's top-right, revealed on `:hover` **and** `:focus-within` so the controls are keyboard-reachable rather than mouse-only:

- the timestamp as a `.readout`, `title` carrying the full locale string
- **copy markdown** and **copy plain text** as separate icon buttons on agent turns
- **one copy button** on user turns, whose text is already plain

What a copy takes from an agent turn is the text the user reads — the concatenated text segments of the turn — not tool calls or thinking blocks. Plain text is produced by walking the parsed markdown AST to its text nodes, reusing the already-bundled parser rather than a regex strip.

Timestamps still come only from server stamps; an optimistic or queued message has none and shows no time.

### 2, 3, 16. Composer and header

The bottom action strip collapses. One **icon-only** button moves inside the textarea's box at bottom-right, with matching right padding so text never runs under it:

- not running → `key-go`, `ArrowUp`
- running → `key-stop`, filled `Square`, and nothing else

`aria-label` and `title` carry the words. The existing comment "an unlabelled icon is a control you have to learn" is retired deliberately: these are the two most-pressed controls in the product and they sit in a fixed position, which is the case where an icon earns its keep.

Enter still queues while running — the server's durable inbox is untouched. The `Enter sends · Shift+Enter newline` hint row goes; the placeholder already explains queueing and the rest moves into the button tooltip.

The header's `session-stop` button is deleted. Its stated justification — "for stopping a turn you have scrolled away from" — was never true: the composer is pinned to the bottom of the pane and never scrolls away.

### 18. Nagging hints removed

- `composer-ask-hint` ("The agent is waiting on an answer — jump to it"), with the `askPending` / `onFocusAsk` plumbing through `Composer` and `SessionView.focusPendingAsk`. Nothing is lost: the card is in the transcript and the header status badge reads `AwaitingInput`.
- the idle "Scroll up for earlier messages" line. The "Loading earlier messages" state stays — that is real.

Kept, because they report state rather than instruct: the `queued-marker` and the progression line.

### 20. Rail state readout

`Sidebar.tsx` — "N running" and "Ready" both go. "N running" restates what is visible immediately below, since every session row carries its own `StatusDot` and status word; "Ready" labels the absence of news. The nameplate becomes the orange `h` mark and HORSIE alone.

The **error** case is kept: when the session-list request fails, a red lamp and "Offline" appear. That is the only place a dead server link shows before the user clicks something and gets an empty page.

### 4, 5. Durable usage and task list

**Root cause, shared.** `useSessionStream` accumulates `usage` and `tasks` only from live SSE events and never seeds either from the durable agent document, although the server already carries both (`AgentDocument.usage`, `AgentDocument.tasks`, `SessionDetail.usageTotal`). After an offload and refresh there are no live events to replay, so `stream.usage` is `{0,0}` — and `ContextGauge` early-returns on `totalTokens <= 0` — while `stream.tasks` is `[]` and `TaskListPanel` early-returns on empty. The comment at `useSessionStream.ts:329` claims both are "seeded by `useAgent`"; they are not.

The fix differs per value, because their freshness requirements differ.

**Usage — delete the accumulator.** Usage is a server-owned cumulative value. `refreshDocuments` already re-fetches the agent document on `StatusChanged`, which the existing comment correctly identifies as the only safe refresh point (`TurnCompleted` fires before the session actor has processed the durable usage push). Reading `mainAgent.usage` and `detail.usageTotal` directly is therefore exactly as fresh as today, correct after a reload by construction, and removes the entire `fromBackfill` double-count bookkeeping. `ContextGauge` stops early-returning on `totalTokens <= 0` and renders whenever the agent document exists.

**Tasks — seed, with live override.** Task lists change mid-turn, so live events must still win. `state.tasks` stays, seeded from `mainAgent.tasks` when the document arrives and no live `TaskListChanged` has been seen for this session — the same guard shape as the existing `seed-queue`, reset on session switch.

**Task panel.** `TaskListPanel` stops returning `null` when empty and gains an empty state. Its collapsed-rail form is removed; visibility moves to a `ListTodo` toggle in the session header's control cluster, carrying a done/total badge when a plan exists. State persists through `usePersistentState`, default closed. Below `lg` it still overlays the transcript rather than stealing a column.

Both bugs get e2e regressions — that is the point of the item.

---

## Group B — New session and agent config

### 6. Runtime roster

`RuntimeRoster` is deleted. The question it answered — "is my laptop connected?" — moves onto the Runtime picker as an amber dot when zero vendors are connected. Same information, no panel.

### 7. Config bar

Every control becomes an icon-only trigger with a corner dot when it holds a non-default value. Order: Runtime, Repos, Skills, MCP, Memory · spacer · **Model, Thinking**. `title` and `aria-label` both carry label plus current value ("Model — claude-sonnet-4-6"), so hover and assistive tech agree. The bar drops from a wrapping multi-row strip to a single 2rem row.

Accepted cost, stated at design time: the model alias now needs a hover to read, and it is the most-checked value on the bar.

### 8. New-session page

Heading and paragraph deleted. Rather than leaving a stark empty field, the config bar and composer are **centred vertically** in the pane instead of bottom-anchored — an empty page reads as deliberate when its one control group is centred, and as broken when it is pushed to the floor.

### 9. Agent edit page

Stops borrowing the bottom-anchored `SessionConfigBar`. The form becomes one panel — Name, Description, then a **Configuration** subsection with the same pickers as labelled rows in the settings-field idiom — and Save/Cancel move inside the form's column, where they currently sit in a separate block outside the scroller.

To serve both surfaces, the picker *bodies* extract into a shared module rendered by both the icon bar and the labelled form rows, rather than giving `SessionConfigBar` a third mode.

---

## Group C — Settings and admin

### 10. GitHub App credentials

The four credential fields (Client ID, Client secret, App ID, Private key) and their save button move from `IntegrationsSettings` to a new **Admin → GitHub App** page. Settings → Integrations keeps connection status, Connect/Disconnect, and the GitHub-tools MCP toggle; when the app is unconfigured it links to the admin page instead of saying "configure below".

`/api/github/app-config` is unchanged. Whether it is already admin-gated server-side is a check, not an assumption — if it is not, this move is presentation-only and should be recorded as such rather than described as a security improvement.

### 11. Models settings

Master-detail. A provider list (name, kind, key-set lamp) with Add and per-row Edit/Delete; selecting a provider reveals its models with the same three actions.

**Every action saves immediately.** Because `SettingsUpdate` replaces whole collections, each action sends the full current providers+models arrays with one item changed — the server contract is untouched and no API work is needed. Consequences: the page loses its Save/Discard header buttons and its `usePublishDirty` registration, showing a transient "Saved" lamp instead.

Deleting a provider that still has models is blocked with a message naming them; the server would otherwise leave models pointing at a provider that no longer exists.

### 12. Alignment

`AccountSettings` is the sole outlier — no `mx-auto max-w-3xl` scroller, and a panel carrying `max-w-2xl`. All pages move to one shared `SettingsPane` wrapper so this cannot drift again.

### 13. Default vendor

The section and its text input go. Each connected vendor row gets a hover-revealed "Make default" button that saves immediately, so this page also loses its batched Save/Discard.

**Accepted loss.** Today the free-text input can name a vendor that has never connected, and the code comment says that is deliberate. A row-only control cannot express that. A configured-but-absent default renders as a ghost row (lamp off, "not connected") so it stays visible and clearable, but a new absent name can no longer be set from the UI.

### 14. Memory

`NewMemoryForm` hides behind an "Add memory" button in the section header; clicking reveals it and focuses the name field, saving or cancelling hides it again.

---

## Group D — Reading

### 17. Ask-card overflow

The bug is `.chip`, which sets `white-space: nowrap`. `AskUserCard.tsx:99` applies `chip text-left` to every choice button — `text-left` implies multi-line intent, `nowrap` overrides it, and a long choice label runs out of the card. Two adjacent holes: the question text and the answer paragraph lack `break-words`, so a long unbroken token (a path or URL, common in these answers) overflows the same way.

Choice buttons get their own wrapping chip variant rather than an override at the call site; question and answer get `break-words`. `.chip` keeps `nowrap`, which is correct for the short tags it was built for.

### 19. Markdown readability

Four separable causes, in impact order.

**Inline code dominates.** `.prose code` carries a border *and* a background *and* a 0.8em size drop. In text where nearly every clause contains an identifier, the line becomes a fence of bordered boxes and the size drop makes the baseline jitter word to word. The border goes, a faint background tint stays, and the size rises to 0.875em so mono sits at the same optical size as the surrounding sans.

**The measure is far too long.** The transcript column is `max-w-[54rem]` (864px) with prose at 15px, putting a full line at roughly 110–120 characters against a comfortable 60–80. The column stays 54rem — tables, code blocks and tool cards want the room — but paragraphs, headings and list text get a ~70ch cap. Text no longer fills the container, which is correct editorial behaviour.

**Headings are mono.** `.prose h1`–`h4` use `--font-mono`; Martian Mono at heading size is wide and slow to scan, and is much of what reads as dated. They move to the sans face with tighter tracking. Mono stays where it means measurement — legends, readouts, identifiers, code.

**Smaller:** `list-style: square` → a quieter marker; paragraph spacing 0.7em → 0.85em; the code block's inset shadow softened, since the recessed background already carries that job.

This is base-layer work, so all four skins inherit it and each then restates prose in its own terms.

---

## Group E — The theme system (item 15)

### Axes

Two orthogonal attributes on `<html>`:

- **skin** — `console` (default) | `paper` | `soft` | `slate`, as `data-skin`
- **mode** — `dark` | `light` | `system`, as the existing `data-theme`

`useTheme` grows to hold both. `horsie-theme` is kept as the mode key so nobody's setting resets; `horsie-skin` is added. The pre-paint script in `index.html` must resolve **both** or every load flashes Console before the skin lands — the same failure the theme script already exists to prevent.

### What makes skins possible

The existing system is already the right seam: tokens plus `@utility` / `@layer components` recipes. Two structural changes:

1. `@theme inline` gains `var()` indirection for fonts and radii, matching what colours already do.
2. Roughly 40 call sites hard-code the console's typography inline (`font-mono text-[11px] uppercase tracking-[0.12em]`, `text-[15px] font-semibold`, `border-b bg-panel`). These move to semantic classes, or a Paper body still wears mono uppercase headings. Mechanical, but the bulk of this item.

Each skin is then a `[data-skin="…"]` block of tokens plus recipe overrides.

### Rule boundaries

DESIGN.md's current Don'ts — no new radius, no fifth shadow, no third font family — are **Console's** rules and bind Console. Each skin carries its own rule set. Four invariants bind every skin without exception:

- material roles hold across modes rather than brightness
- status is a lamp **and** a word, never colour alone
- syntax highlighting keeps its own `--code-*` palette
- AA is measured by `scripts/contrast.mjs`, not assumed

### The three worlds

Renditions chosen against the calibration warnings in `new-work.md` §63–67, which name "warm cream ground + serif display + terracotta accent" as an AI-default cluster and list Newsreader, Inter, IBM Plex, DM Sans and Space Grotesk as default faces.

**Paper — editorial calm.** No panel borders; separation is whitespace plus one hairline rule where structure demands it. A **cool neutral** ground, not cream — paper under daylight rather than lamplight, which is what dodges the cluster. Humanist sans throughout at two weights, no serif display at all; sentence-case labels replacing uppercase mono legends. Flat surfaces, 4px radii, no shadow outside popovers. Accent is **ink blue**, not terracotta. Face: **Libre Franklin**.

**Soft — modern product surface.** Borders replaced by elevation; 10–14px radii, layered **warm** neutral greys rather than the blue-greys that are the SaaS tell. Geometric sans, sentence-case labels, soft fills with no key travel, lamps as flat dots without glow. Accent: muted violet. Face: **Manrope**.

**Slate — minimal monochrome.** Reductive: no borders, no shadows, separation by background steps alone. True neutral greys, one accent, 6px radii. Uppercase gone; mono retained only for code and identifiers. Reuses **Archivo**, already bundled — the reductive skin reusing the incumbent face is on-concept and costs nothing.

### Font budget

Two new self-hosted families, Libre Franklin and Manrope, both **lazy-loaded when their skin activates**, so Console's initial bundle is unchanged. Martian Mono is reused as the mono across all four skins at per-skin settings — `wdth 100` and normal tracking tame it considerably, and the mannered feel comes from the `wdth 87.5` + `0.1em` + uppercase combination rather than the face itself. No CDN, per the LAN constraint.

### Colour sets — scope change

The design discussion included a third axis: three curated accent sets re-tinting the commit and live hues. **This is dropped**, deliberately, and the reason belongs on the record: each skin already ships its own light and dark palette with its own accent hue, which is eight distinct palettes and delivers "a different colour set" through the skin choice itself. A third orthogonal axis would take the contrast matrix from 8 cells to 24 for marginal user value. If accents are still wanted they are a clean follow-up on top of this structure.

### Appearance page

A new **Settings → Appearance** owns skin and mode, with a live preview swatch per skin and a Light/Dark/System control (System is new). The sidebar `ThemeToggle` stays as the quick mode flip. `SettingsMenu` keeps the per-view display toggles (`showThinking`) — those are transcript options, not appearance.

### Direction contract

`index.html` currently carries a single-world contract naming Console. It becomes a four-world block, one per skin, and must survive the production build — grep `dist/index.html` for the seed key after building.

**No concept roll was run.** `new-work.md` §3 requires `concept-seed` before code on a new world, and §49 states that a user- or brief-pinned direction beats the roll. The user was shown four named directions with described characters and selected three; that is the pin. The roll selects a direction, and the direction was already chosen.

---

## Testing

**Unit:** the markdown→plaintext helper, skin and mode persistence in `useTheme`, the per-item models mutation payload builder, the task-seeding guard.

**e2e** (`clients/web/e2e`, which boots a real server, mock LLM and real runtime daemon). These existing specs break and need updating: `session-stop`, `turn-time`, the `config-*` label assertions, `task-list-expand`, the Models save button, and the Runtimes default-vendor input.

New coverage:

- task panel toggle persists across reload and shows its empty state
- **usage survives a reload after offload**
- **tasks survive a reload after offload**
- skin switch applies `data-skin`, survives reload, and does not flash on load

**Contrast:** `scripts/contrast.mjs` extended to walk 4 skins × 2 modes, gating on AA for every ink against every field.

**Screenshots:** the throwaway `zz-shots.spec.ts` route, both modes × four skins, deleted before commit.

## Risks and accepted losses

1. **One PR is a very large diff** — every session surface, five settings pages, a new admin page, a theming layer, and ~40 call-site migrations. Chosen by the user after the size was flagged.
2. **Icon-only Model** means the model alias needs a hover to read. The change most likely to want reverting.
3. **Default vendor** can no longer be set by name for an unconnected vendor.
4. **Accent axis dropped** from the theming scope, as recorded above.
5. Item 15 is roughly half the work.
