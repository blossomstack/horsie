---
name: horsie
description: One quiet surface for supervising long-running agent sessions — nothing separated by a line, a fill only where something happened, one accent.
colors:
  chassis: "oklch(0.166 0.005 268)"
  panel: "oklch(0.188 0.005 268)"
  panel-raised: "oklch(0.245 0.006 268)"
  screen: "oklch(0.148 0.005 268)"
  rule: "oklch(0.268 0.007 268)"
  rule-strong: "oklch(0.36 0.009 268)"
  legend: "oklch(0.968 0.003 268)"
  legend-dim: "oklch(0.755 0.009 268)"
  legend-faint: "oklch(0.678 0.011 268)"
  keycap: "oklch(0.296 0.009 268)"
  keycap-hover: "oklch(0.34 0.01 268)"
  keycap-ink: "oklch(0.955 0.003 268)"
  accent: "oklch(0.605 0.212 275)"
  accent-hover: "oklch(0.655 0.2 275)"
  accent-ink: "oklch(0.985 0.008 275)"
  accent-quiet: "oklch(0.3 0.09 275)"
  live: "oklch(0.82 0.14 82)"
  live-ink: "oklch(0.82 0.14 82)"
  live-quiet: "oklch(0.32 0.06 82)"
  red: "oklch(0.615 0.216 25)"
  red-ink: "oklch(0.72 0.185 25)"
  red-quiet: "oklch(0.31 0.085 25)"
  lamp-ok: "oklch(0.795 0.155 158)"
  lamp-ok-quiet: "oklch(0.31 0.065 158)"
  code-keyword: "oklch(0.775 0.135 285)"
  code-string: "oklch(0.82 0.125 160)"
  code-number: "oklch(0.845 0.115 82)"
  code-type: "oklch(0.815 0.1 225)"
  focus-ring: "oklch(0.665 0.212 275 / 0.6)"
typography:
  page-title:
    fontFamily: "Inter Variable, system-ui, sans-serif"
    fontSize: "0.9375rem"
    fontWeight: 600
    letterSpacing: "-0.018em"
    lineHeight: 1.25
  section-title:
    fontFamily: "Inter Variable, system-ui, sans-serif"
    fontSize: "0.8125rem"
    fontWeight: 600
    letterSpacing: "-0.008em"
    lineHeight: 1.3
  item-title:
    fontFamily: "Inter Variable, system-ui, sans-serif"
    fontSize: "0.8438rem"
    fontWeight: 550
    letterSpacing: "-0.008em"
    lineHeight: 1.3
  body:
    fontFamily: "Inter Variable, system-ui, sans-serif"
    fontSize: "0.9375rem"
    fontWeight: 400
    lineHeight: 1.55
  ui:
    fontFamily: "Inter Variable, system-ui, sans-serif"
    fontSize: "0.8125rem"
    fontWeight: 400
  legend:
    fontFamily: "Inter Variable, system-ui, sans-serif"
    fontSize: "0.6875rem"
    fontWeight: 500
    lineHeight: 1.3
    letterSpacing: "0"
  readout:
    fontFamily: "Geist Mono Variable, ui-monospace, SF Mono, monospace"
    fontSize: "0.8125rem"
    letterSpacing: "-0.01em"
    fontFeature: "tabular-nums"
  key:
    fontFamily: "Inter Variable, system-ui, sans-serif"
    fontSize: "0.8125rem"
    fontWeight: 550
    letterSpacing: "-0.006em"
  chip:
    fontFamily: "Inter Variable, system-ui, sans-serif"
    fontSize: "0.6875rem"
    fontWeight: 500
    letterSpacing: "0"
  code:
    fontFamily: "Geist Mono Variable, ui-monospace, SF Mono, monospace"
    fontSize: "0.8125rem"
    lineHeight: 1.5
  field:
    fontFamily: "Inter Variable, system-ui, sans-serif"
    fontSize: "0.875rem"
    fontWeight: 400
    lineHeight: 1.45
  kbd:
    fontFamily: "Geist Mono Variable, ui-monospace, SF Mono, monospace"
    fontSize: "0.625rem"
rounded:
  chip: "5px"
  control: "7px"
  cap: "7px"
  panel: "11px"
  # Reserved single-purpose radii. Not general-purpose steps: each belongs to
  # exactly one part and nothing else may reach for it.
  focus: "2px"
  lamp: "999px"
  scrollbar: "999px"
components:
  key:
    backgroundColor: "{colors.keycap}"
    textColor: "{colors.keycap-ink}"
    typography: "{typography.key}"
    rounded: "{rounded.cap}"
    padding: "0.375rem 0.75rem"
  key-hover:
    backgroundColor: "{colors.keycap-hover}"
  key-go:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.accent-ink}"
    typography: "{typography.key}"
    rounded: "{rounded.cap}"
    padding: "0.375rem 0.75rem"
  key-go-hover:
    backgroundColor: "{colors.accent-hover}"
  key-stop:
    backgroundColor: "{colors.red}"
    textColor: "{colors.accent-ink}"
    typography: "{typography.key}"
    rounded: "{rounded.cap}"
    padding: "0.375rem 0.75rem"
  key-blank:
    backgroundColor: "transparent"
    textColor: "{colors.legend-dim}"
    typography: "{typography.key}"
    rounded: "{rounded.cap}"
    padding: "0.375rem 0.75rem"
  key-flat:
    backgroundColor: "transparent"
    textColor: "{colors.legend-dim}"
    typography: "{typography.key}"
    rounded: "{rounded.cap}"
    padding: "0.375rem 0.75rem"
  key-danger:
    backgroundColor: "transparent"
    textColor: "{colors.red-ink}"
    typography: "{typography.key}"
    rounded: "{rounded.cap}"
    padding: "0.375rem 0.75rem"
  key-icon:
    backgroundColor: "transparent"
    textColor: "{colors.legend-dim}"
    rounded: "{rounded.control}"
    height: "1.75rem"
    width: "1.75rem"
  field:
    backgroundColor: "{colors.screen}"
    textColor: "{colors.legend}"
    rounded: "{rounded.control}"
    padding: "0.375rem 0.625rem"
    size: "0.875rem"
  field-mono:
    typography: "{typography.code}"
    size: "0.8125rem"
  row:
    backgroundColor: "transparent"
    textColor: "{colors.legend}"
    rounded: "{rounded.control}"
    padding: "0.375rem 0.625rem"
  row-hover:
    backgroundColor: "{colors.panel-raised}"
  panel:
    backgroundColor: "{colors.panel}"
    textColor: "{colors.legend}"
    rounded: "{rounded.panel}"
  section:
    backgroundColor: "transparent"
    textColor: "{colors.legend}"
  floating:
    backgroundColor: "{colors.panel}"
    textColor: "{colors.legend}"
    rounded: "{rounded.panel}"
  screen:
    backgroundColor: "{colors.screen}"
    textColor: "{colors.legend-dim}"
    rounded: "{rounded.control}"
  chip:
    backgroundColor: "{colors.screen}"
    textColor: "{colors.legend-dim}"
    typography: "{typography.chip}"
    rounded: "{rounded.chip}"
    padding: "0.0625rem 0.4375rem"
  kbd:
    backgroundColor: "{colors.panel-raised}"
    textColor: "{colors.legend-faint}"
    rounded: "4px"
    padding: "0.0625rem 0.3125rem"
    size: "0.625rem"
  lamp:
    height: "7px"
    width: "7px"
    rounded: "999px"
---

# Design System: horsie

## Overview

**Creative North Star: "One Surface"**

horsie is a long-running process you watch. The interface's whole job is to show its state and then get out of the way, so it is built as a single quiet surface rather than as a stack of boxes. There is no chassis, no bezel and no card: a session, a settings page and a roster are all the same sheet, and what separates their regions is a small step in value, whitespace, and type weight.

Three rules generate the rest of this document.

**1. Nothing is separated by a line.** A 1px rule around a region is a boundary drawn twice — the region already has an edge, because its content stops. A screen full of them reads as clutter before it reads as structure. The only borders left in the build are the ones that carry meaning on their own: an error outline, the focus ring, the sub-session tree rail, a graph edge, a dashed timeline guide. `--rule` still exists for those; it is not a tool for bounding regions.

**2. A fill means something happened.** Ground is ground. A fill marks hover, selection, or machine output — and nothing else. When every list row is painted a permanent card, the fill that means "you are hovering this" has nowhere left to land, which is exactly the failure the previous system shipped.

**3. One accent.** `--accent` is the control that commits and nothing else: Send, Save, Create, New agent. `--live` is a measured value in flight. `--red` interrupts and destroys. `--lamp-ok` says a channel is healthy. A fifth colour would have to displace one of these.

Light and dark are the same design at two exposures, not two designs.

**Key characteristics:**
- Two self-hosted faces: **Inter** for everything read, **Geist Mono** for identifiers, paths and code. No font ever comes from a CDN — horsie servers routinely run on LANs with no route to the public internet.
- Titles carry negative tracking, labels carry none. Sentence case throughout; there are no engraved uppercase legends.
- The density is deliberately tight — this is a transcript you scan, not an essay you settle into.
- A status is always a lamp **and** a word. Never colour alone.
- Every ink clears WCAG AA against every surface it can land on, in all four worlds and both exposures — measured by `clients/web/scripts/contrast.mjs`, not assumed.

## Themes

Four worlds ship over the same layouts, chosen on Settings → Appearance and applied as `data-skin` on `<html>`:

| | Character | Face | Accent |
| --- | --- | --- | --- |
| **Graphite** *(default)* | Cool graphite, the reference world | Inter | Electric indigo |
| **Ink** | Bright minimal, true neutral | Geist | Ink itself — near-black on white, near-white on black |
| **Aurora** | Every ground tinted with the accent's own hue | Plus Jakarta Sans | Mint |
| **Glass** | Frosted sheets over a tinted, back-lit ground | Inter | Azure |

Graphite is the default and **carries no attribute**, so `index.css` keeps the specificity it was written against; the other three live in `clients/web/src/skins.css`.

**A world replaces material, never structure.** Same components, same layout, same `data-testid`s, same positions. A change that moves an element is a change to all four.

**What a world may vary** reaches the app through the seam declared on `:root` in `index.css`: `--face-sans` / `--face-mono`, the `--r-*` radius scale, `--float`, and the palette. `@theme inline` resolves the Tailwind aliases through those, so a world re-roles the whole app by declaring variables. Anything a world needs to vary that is *not* on that list needs a **new seam variable, never a call-site override** — a rule that has been broken every time it was not written down.

**The seam defaults live in `@layer base`.** `@import "./skins.css"` must sit at the top of `index.css` (CSS requires imports first), which puts the world blocks *earlier* in source order at equal specificity. Layering settles it by precedence rather than by specificity, which also keeps `[data-skin]` working on a plain element — the Appearance page's preview swatches set it on a `div` to render each world from its own tokens, and an `html[data-skin]` selector would never match them.

**Four invariants bind every world:**

1. Nothing is separated by a line, a fill means something happened, one accent. A world that reintroduces a border is not a world, it is a different design.
2. A status is a lamp **and** a word.
3. Syntax highlighting keeps its own `--code-*` palette, distinct from the semantic four.
4. AA is **measured**. `contrast.mjs` parses the shipped CSS and **discovers** the worlds by scanning for `data-skin` blocks rather than carrying a list — a hardcoded list is blind to exactly the case the gate exists for.

Each world declares its **full** palette per exposure rather than patching Graphite's; a half-inherited ramp would be verified against numbers it does not use. The two extra faces are lazy-loaded by `useTheme` when their world is chosen, so Graphite's initial bundle is unchanged by the existence of the others — and Glass deliberately reuses Inter, so it costs no fetch.

## Colors

Two full renditions live in `clients/web/src/index.css`: dark on `:root, [data-theme="dark"]` (the primary rendition — the scene is a dim room at 11pm) and light on `[data-theme="light"]`. The frontmatter above carries the dark values; the light counterparts are declared token-for-token in the same file and are normative there. Tailwind aliases are exposed via `@theme inline` (`bg-panel`, `bg-raised`, `bg-screen`, `text-legend`, `text-dim`, `text-faint`, `text-accent`, `text-live-ink`, `text-red-ink`, `text-lamp-ok`, …) — use those, never a raw hex or a Tailwind palette colour.

### The four surfaces

Four values, and the steps between them are deliberately small.

- **`chassis`** — the frame: the session rail, the settings nav. The recessed column you navigate from.
- **`panel`** — the surface you read: the content column, and anything that genuinely floats above it.
- **`panel-raised`** — **the interaction fill.** Hover, selection, a menu. This is the one token whose *brightness* flips between exposures: lighter in the dark, **darker** in the light. What it has to do is separate from the ground it lands on, and a white fill on a white panel separates from nothing. The previous system held material roles by brightness in both exposures, which is precisely why every light-mode hover state was invisible.
- **`screen`** — machine output: tool results, code blocks, log tails, and the fill inside a field. A tint that says "not written by a person", not a recessed hardware screen.

The rails are `chassis` and the content is `panel`, in that order. Painting the rail *lighter* than the content — which is what shipped before — inverts the reading and makes the navigation the loudest thing on screen.

### Semantic colour
- **Accent** (`accent`, `accent-hover`, `accent-ink`, `accent-quiet`): the action that commits. `.key-go` (composer Send, settings Save, section Add, New agent, ask-user Send), the checked state of a display switch, the `h` nameplate, the focus ring, and prose links. Nowhere else.
- **Live** (`live`, `live-ink`, `live-quiet`): a measured value in flight. Token counts, timers, the context meter fill, the "Running" / "Reconnecting" / "Saving" lamps, a selected weekday. `live-ink` is the *text* form (in light it darkens to clear AA); `live` is the *emissive* form used for lamps and meters.
- **Red** (`red`, `red-ink`, `red-quiet`): interrupt and destroy. `.key-stop`, delete hover, error banners, failed tool rows. Red is never used to style anything that is merely important.
- **Lamp OK** (`lamp-ok`, `lamp-ok-quiet`): the only "all good" signal — an idle session, a connected runtime, a completed task, a tool call that returned.
- **Code** (`code-keyword`, `code-string`, `code-number`, `code-type`): its own palette, deliberately. Driving syntax from `--accent` puts the colour that means "this commits" on every `let` and `fn`.

## Typography

One reading face and one machine face, both self-hosted.

**Inter** carries the interface. It was drawn for exactly this — dense UI at small sizes with a tall x-height — and the build turns on its own `cv11` and `ss01` features, which is what stops `Il1` collapsing in a path. **Geist Mono** carries identifiers, paths, code and readouts.

Three title roles, one rendition each, so the same job never ships in two looks: `.page-title` (the h1 of a pane), `.section-title` (a block heading), `.item-title` (the name of one row). All three carry negative tracking around `-0.01em`; `.legend`, the small-label role, carries none. That contrast — tight titles, neutral labels — is most of what separates a current interface from a dated one, and it costs nothing.

**A name is not a machine string.** `.item-title` is the reading face. The previous system rendered every row name in mono, which was right when a row labelled a channel on an instrument face and wrong here, where it is the row's heading and belongs to the same document as the prose around it.

Prose rhythm is deliberately tighter than a web article's: `1.55` line-height, `0.6em` paragraph gaps, `0.15em` list items. A transcript is scanned for a specific thing, not settled into; at 1.65/0.85em a four-paragraph reply filled a screen and you scrolled past what you could have taken in at a glance.

## Layout

A three-column desk: **session rail** (17.5rem / `w-[17.5rem]`, `bg-chassis`) — **content** (fluid, `min-w-0 flex-1`, `bg-panel`) — **task panel** (16rem / `w-64`, present only once the agent has used the task tool). Settings and Admin substitute their own 12rem (`w-48`) nav column, also on `bg-chassis`. None of the three is ruled off; the value step is the separation.

**One header height, `--header-h` (2.75rem).** Every column header reads the token, so the app lines up across its seams without any of them agreeing on a number separately.

Content columns are capped, not fluid:
- **54rem** (`max-w-[54rem]`) — the transcript, composer, config bar and progression rows. Everything in a session shares one centred column so the recording reads as one strip.
- **48rem** (`max-w-3xl`) — settings and admin content.
- **`--prose-measure` (70ch)** — running text inside a reply, and only running text. Tables and code blocks keep the full column.

Settings sections are separated by `space-y-7` and nothing else. That gap is load-bearing now: it is doing the job the panel border used to.

**The sidebar footer carries the scope.** The project switcher sits on one strip with icon-only Settings and Admin and the exposure toggle. The switcher used to sit under the nameplate with the word "Project" over it — two rows of rail height for one string that is also in the URL.

**Breakpoints are Tailwind v4 defaults**; no custom breakpoints are declared. Four responsive rules carry the layout:

- **Below `md`, the session rail becomes a drawer** — `fixed inset-y-0 left-0`, sliding on `translate-x` with `--float` and a scrim. Pages render their own `<RailToggle/>` in their header. It closes on route change and on Escape. At 390px a persistent column would eat two thirds of the viewport.
- **Below `md`, the settings nav becomes a scrolling strip** — a horizontal `overflow-x-auto` row of keys with a right-edge fade mask so it reads as scrollable.
- **Below `lg`, the task panel overlays and starts collapsed** — `absolute inset-y-0 right-0 z-20` with `--float`, collapsed to an icon strip showing a `done/total` readout. Hiding it outright left narrow screens with a plan they could not ask for.
- **Below `sm`, the transcript gutter goes inline** — the right-aligned channel/timestamp column becomes a single row above the turn's content.

## Elevation & Depth

**There is one shadow token, and only things that genuinely float get it.** `--float` is spent on menus, popovers, dialogs, and the two mobile overlays. Nothing else in the build has elevation: the content column does not float above the rail, a settings section does not float above the page, and a list row does not float above the list.

This replaced a four-token vocabulary — cap lift, cap flat, panel lift, screen inset — that existed to model a physical instrument. Modelling depth on a surface that has none is what produced the boxes.

**The Visible-Focus Rule.** Keyboard focus must be *seen*, and `--focus-ring` is often the only indicator a control has. It is semi-transparent, so what counts is the composite over the surface behind it, and `contrast.mjs` gates that at 3:1 (WCAG 1.4.11) across all eight palettes — as it does the `accent-ink`-on-`accent` ring drawn *inside* the commit key. A ring you cannot measure is a ring nobody checked.

## Shapes

Four radii: **5px** (`--radius-chip`), **7px** (`--radius-control`), **7px** (`--radius-cap`), **11px** (`--radius-panel`). Reach them as `var(--radius-*)` or `rounded-[var(--radius-control)]`. Each world sets its own scale through the same four variables — Aurora opens to 7/10/10/16, Glass to 8/11/11/18.

Three radii sit outside that scale and are reserved to exactly one part each: the **lamp** is a 7px circle, `:focus-visible` normalises to **2px** so the outline traces a control tightly regardless of what it wraps, and the **scrollbar thumb** is a pill. They are recorded in the `rounded` scale so the system is auditable, not so they are available.

`* { border-color: var(--rule) }` is still set globally in `@layer base` — in a layer on purpose, because an unlayered rule beats every author layer regardless of specificity, and while this sat outside a layer it silently killed every `border-red` on an error banner.

## Components

The vocabulary is defined once in `clients/web/src/index.css` as Tailwind `@utility` (`panel`, `section`, `floating`, `screen`, `legend`, `readout`) and `@layer components` classes (everything else). Build new surfaces out of these; do not restyle them locally.

### Surfaces
- **`.section`** — a region grouping content on the page: a settings block, a roster. No fill, no line; whitespace and a title do the separating. This is what replaced `.panel p-4` on every settings page.
- **`.panel`** — a contained region with a fill and a radius, no border.
- **`.floating`** — a menu, popover or dialog. The only place `--float` is spent.
- **`.screen`** — machine output.

### Controls
- **`.key`** — a filled rectangle with a word on it. No travel, no moulded edge, no engraved legend. `.key-go` commits, `.key-stop` interrupts, `.key-blank` is the ghost secondary, `.key-flat` is bare responsive text, `.key-danger` destroys, `.key-icon` is the 1.75rem square.
- **`.field`** — a filled slot, not an outlined box. The fill is what says "you can type here"; a border would say it a second time. Focus adds the accent ring.
- **`.row`** — a list entry: padding and a hover fill. Not a card. The card-per-row pattern spent four separations — a fill, a ring, a gap and a radius — on one boundary.

### Indicators
- **`.lamp`** — 7px, always paired with a word. `.lamp-live` breathes; `.lamp-off` is a ring.
- **`.chip`** — a small filled tag, no border.
- **`.readout`** — a live measured value, tabular so digits never jitter as they tick.

## Do's and Don'ts

**Do** separate regions with a value step, whitespace, or type weight.
**Don't** reach for a border. If a region needs bounding, it needs space.

**Do** spend a fill on hover, selection, or machine output.
**Don't** paint a region just to show it is a region — it costs the fills that mean something.

**Do** add a seam variable when a world needs to vary something.
**Don't** override at the call site. Every call-site override is a world that will silently look wrong.

**Do** use `.item-title` for the name of a row, in the reading face.
**Don't** reach for `font-mono` unless the string is genuinely a machine string — a path, an id, a model alias, code.

**Do** run `bun run contrast` after any token change. It gates all eight palettes and it reads the shipped CSS.
**Don't** lower a threshold to make it pass. Both times a value failed the gate, the value was wrong.

**Do** pair every status colour with a word.
**Don't** signal state with colour alone, in any world.
