---
name: horsie
description: A creator-hardware desk instrument for supervising long-running agent sessions.
colors:
  chassis: "oklch(0.19 0.005 255)"
  panel: "oklch(0.235 0.006 255)"
  panel-raised: "oklch(0.285 0.007 255)"
  screen: "oklch(0.155 0.006 255)"
  rule: "oklch(0.325 0.008 255)"
  rule-strong: "oklch(0.43 0.01 255)"
  legend: "oklch(0.93 0.008 90)"
  legend-dim: "oklch(0.71 0.012 90)"
  legend-faint: "oklch(0.655 0.013 90)"
  keycap: "oklch(0.82 0.016 90)"
  keycap-hover: "oklch(0.87 0.016 90)"
  keycap-ink: "oklch(0.22 0.008 255)"
  keycap-edge: "oklch(0.63 0.016 90)"
  orange: "oklch(0.688 0.196 42)"
  orange-hover: "oklch(0.735 0.19 42)"
  orange-ink: "oklch(0.19 0.02 42)"
  orange-quiet: "oklch(0.32 0.075 42)"
  amber: "oklch(0.8 0.155 78)"
  amber-ink: "oklch(0.8 0.155 78)"
  amber-quiet: "oklch(0.33 0.07 78)"
  red: "oklch(0.655 0.215 27)"
  red-ink: "oklch(0.7 0.2 27)"
  red-quiet: "oklch(0.31 0.09 27)"
  lamp-ok: "oklch(0.78 0.16 158)"
  lamp-ok-quiet: "oklch(0.32 0.07 158)"
  code-keyword: "oklch(0.78 0.15 40)"
  code-string: "oklch(0.8 0.13 155)"
  code-number: "oklch(0.82 0.13 78)"
  code-type: "oklch(0.8 0.1 220)"
  focus-ring: "oklch(0.8 0.155 78 / 0.55)"
typography:
  title:
    fontFamily: "Archivo Variable, system-ui, sans-serif"
    fontSize: "15px"
    fontWeight: 600
    letterSpacing: "-0.025em"
  body:
    fontFamily: "Archivo Variable, system-ui, sans-serif"
    fontSize: "0.9375rem"
    fontWeight: 400
    lineHeight: 1.65
  ui:
    fontFamily: "Archivo Variable, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1.25rem
  legend:
    fontFamily: "Martian Mono Variable, ui-monospace, SF Mono, monospace"
    fontSize: "10px"
    fontWeight: 500
    lineHeight: 1.4
    letterSpacing: "0.1em"
    fontVariation: "'wdth' 100"
  readout:
    fontFamily: "Martian Mono Variable, ui-monospace, SF Mono, monospace"
    fontSize: "13px"
    letterSpacing: "0.01em"
    fontFeature: "tabular-nums"
    fontVariation: "'wdth' 87.5"
  key:
    fontFamily: "Martian Mono Variable, ui-monospace, SF Mono, monospace"
    fontSize: "11px"
    fontWeight: 550
    letterSpacing: "0.06em"
    fontVariation: "'wdth' 87.5"
  chip:
    fontFamily: "Martian Mono Variable, ui-monospace, SF Mono, monospace"
    fontSize: "10px"
    fontWeight: 500
    letterSpacing: "0.04em"
    fontVariation: "'wdth' 87.5"
  code:
    fontFamily: "Martian Mono Variable, ui-monospace, SF Mono, monospace"
    fontSize: "11px"
    lineHeight: 1.625
    fontVariation: "'wdth' 87.5"
  field:
    fontFamily: "Archivo Variable, system-ui, sans-serif"
    fontSize: "0.875rem"
    fontWeight: 400
  kbd:
    fontFamily: "Martian Mono Variable, ui-monospace, SF Mono, monospace"
    fontSize: "9px"
    letterSpacing: "0.04em"
rounded:
  chip: "3px"
  control: "4px"
  cap: "6px"
  panel: "8px"
  # Reserved single-purpose radii. Not general-purpose steps: each belongs to
  # exactly one part and nothing else may reach for it.
  scrollbar: "1px"
  focus: "2px"
  lamp: "999px"
components:
  key:
    backgroundColor: "{colors.keycap}"
    textColor: "{colors.keycap-ink}"
    typography: "{typography.key}"
    rounded: "{rounded.cap}"
    padding: "8px 14px"
  key-hover:
    backgroundColor: "{colors.keycap-hover}"
  key-go:
    backgroundColor: "{colors.orange}"
    textColor: "{colors.orange-ink}"
    typography: "{typography.key}"
    rounded: "{rounded.cap}"
    padding: "8px 14px"
  key-go-hover:
    backgroundColor: "{colors.orange-hover}"
  key-stop:
    backgroundColor: "{colors.red}"
    textColor: "{colors.orange-ink}"
    typography: "{typography.key}"
    rounded: "{rounded.cap}"
    padding: "8px 14px"
  key-blank:
    backgroundColor: "transparent"
    textColor: "{colors.legend-dim}"
    typography: "{typography.key}"
    rounded: "{rounded.cap}"
    padding: "8px 14px"
  key-flat:
    backgroundColor: "transparent"
    textColor: "{colors.legend-dim}"
    typography: "{typography.key}"
    rounded: "{rounded.cap}"
    padding: "8px 14px"
  key-danger:
    backgroundColor: "transparent"
    textColor: "{colors.red-ink}"
    typography: "{typography.key}"
    rounded: "{rounded.cap}"
    padding: "8px 14px"
  key-icon:
    backgroundColor: "transparent"
    textColor: "{colors.legend-dim}"
    rounded: "{rounded.control}"
    height: "2rem"
    width: "2rem"
  field:
    backgroundColor: "{colors.screen}"
    textColor: "{colors.legend}"
    rounded: "{rounded.control}"
    padding: "8px 11px"
    size: "0.875rem"
  field-mono:
    typography: "{typography.code}"
    size: "0.8125rem"
  panel:
    backgroundColor: "{colors.panel}"
    textColor: "{colors.legend}"
    rounded: "{rounded.panel}"
  screen:
    backgroundColor: "{colors.screen}"
    textColor: "{colors.legend-dim}"
    rounded: "{rounded.control}"
  chip:
    backgroundColor: "{colors.panel-raised}"
    textColor: "{colors.legend-dim}"
    typography: "{typography.chip}"
    rounded: "{rounded.chip}"
    padding: "2px 7px"
  kbd:
    backgroundColor: "{colors.panel-raised}"
    textColor: "{colors.legend-faint}"
    rounded: "{rounded.chip}"
    padding: "1px 5px"
    size: "9px"
  lamp:
    height: "7px"
    width: "7px"
    rounded: "999px"
---

# Design System: horsie

## Overview

**Creative North Star: "The Console"**

horsie is an instrument you leave running, so the interface is built as the front panel of one: a machined gunmetal chassis, keycaps you press, dye-sublimated mono legends naming each channel, and recessed screens where the machine's own output appears. It is not a chat app with a dark theme. The transcript reads down a single stamped gutter rather than zig-zagging between bubbles, every live number is an amber readout rather than body text, and there is exactly one safety-orange control on any given screen — the one that commits.

Colour is load-bearing and never decorative. Four semantic families do all the work: **amber** is a measured, live value; **safety orange** is the action that commits (send, save, create); **red** is interrupt and destroy; **green** is the one lamp that says a channel is healthy. Everything else is material — chassis, panel, raised panel, screen — plus three weights of engraved ink. If a colour on a new surface does not fall into one of those buckets, the surface is wrong, not the palette.

The system is deliberately dense and unrounded. Radii are machined (3/4/6/8px), not pill-shaped; shadows do one of two things (lift a cap out, or recess a screen in) and never diffuse. The one place the instrument steps back is agent prose, which runs in the proportional face at a real reading measure — the transcript is the product, and the chrome exists to be read past.

**Key Characteristics:**
- Two self-hosted faces: Martian Mono for legends, readouts, keys and machine strings; Archivo for prose and UI. No third face, no font CDN.
- Four semantic colours (amber / orange / red / green), each with one meaning, plus a separate four-colour syntax palette.
- Two themes that swap material *roles*, not brightness — a keycap is always a different material from its panel.
- Machined radii 3–8px; no pills except the 7px lamp.
- A status is always a lamp **and** a word. Never colour alone.
- Every ink clears WCAG AA against every field colour in both themes.

## Colors

Two full renditions live in `clients/web/src/index.css`: dark on `:root, [data-theme="dark"]` (the primary rendition — the scene is a dim room at 11pm) and light on `[data-theme="light"]`. The frontmatter above carries the dark values; the light counterparts are declared token-for-token in the same file and are normative there. Tailwind aliases are exposed via `@theme inline` (`bg-panel`, `bg-raised`, `bg-screen`, `text-legend`, `text-dim`, `text-faint`, `text-amber-ink`, `text-red-ink`, `text-lamp-ok`, `border-rule`, …) — use those, never a raw hex or a Tailwind palette colour.

### Primary
- **Safety Orange** (`orange`, `orange-hover`, `orange-ink`, `orange-quiet`): the action that commits. It appears on `.key-go` (composer Send, settings Save, section Add, New agent, ask-user Send), on the checked state of a display switch, on the `h` nameplate cap, and as the "Awaiting input" lamp tone — a question waiting for you to commit an answer. Nowhere else. In light it also tints the 3px focus halo (`focus-ring`), which is amber in dark.

### Secondary
- **Instrument Amber** (`amber`, `amber-ink`, `amber-quiet`): a measured, live value. Token counts, timers, the context-window meter fill, the streaming `.caret`, the "Running"/"Reconnecting"/"Saving" lamps, the focused field's border and the global `:focus-visible` outline in both themes. `amber-ink` is the *text* form (in light it darkens to clear AA); `amber` is the *emissive* form used for lamps, meters and rings. `amber-quiet` is the wash behind an ask-user card and `::selection`.

### Tertiary
- **Emergency Red** (`red`, `red-ink`, `red-quiet`): interrupt and destroy. `.key-stop`, the delete hover state, error banners, failed tool rows, the unrecoverable-session block. Red is never used to style anything that is merely important.
- **Ready Green** (`lamp-ok`, `lamp-ok-quiet`): the only "all good" signal — an idle session's lamp, a connected runtime, a completed task, a saved settings page, a tool call that returned.

### Neutral
- **Gunmetal Chassis** (`chassis`): the physical ground. The body background, the transcript's own field, and the settings nav column.
- **Front Panel** (`panel`): the sidebar, headers, popovers, cards, the composer shell — anything that is a face you read controls off.
- **Lifted Panel** (`panel-raised`): a panel pushed toward you. Hover states, selected rows, user-message blocks, chips, `kbd`, settings row shells.
- **Recessed Screen** (`screen`): machine output. Tool input/output, thinking text, code blocks, empty-state wells, and every `.field` — a field is a slot cut into the panel, not a box floating on it.
- **Panel Rule / Strong Rule** (`rule`, `rule-strong`): the global default border colour (`* { border-color: var(--rule) }`) and the heavier one used for scrollbar thumbs, blockquote bars and the inset ring on a selected row.
- **Legend / Dim / Faint** (`legend`, `legend-dim`, `legend-faint`): engraved ink in three weights — primary text, secondary text, and label/placeholder text. `legend-faint` is the floor: at 4.53:1 on `panel-raised` in dark it is the worst case in the system and still clears AA.
- **Keycap / Keycap Ink / Keycap Edge** (`keycap`, `keycap-hover`, `keycap-ink`, `keycap-edge`): the material of anything you press, and the 1px hard edge under it that collapses when pressed.

### Named Rules

**The One Orange Rule.** Safety orange marks the control that commits, and nothing else. If a screen has two orange keys, one of them is wrong. A destructive confirm is red, a secondary action is `.key-blank`, an inline action is `.key-flat`.

**The Lamp-Plus-Word Rule.** A `.lamp` never carries meaning alone. Every one in the build sits beside its word — "Idle", "Running", "Reconnecting", "Saved", "Unsaved", "Offline", "3 running" — or has an `sr-only` word if the visual pairing is an icon. `aria-hidden` on the lamp, the word does the talking.

**The Separate Phosphor Rule.** Syntax highlighting is driven by `code-keyword` / `code-string` / `code-number` / `code-type`, which are a *different palette* from the control colours. Driving highlighting from `--orange` put safety orange on every `let` and `fn` and spent the one colour that means "this commits". Never re-point `.hljs-*` at `--orange` or `--amber`.

**The Material-Role Rule.** Material roles hold their meaning across themes, not their brightness. `screen` is always the most recessed, `panel-raised` is always the most lifted, and a keycap is always a *different material* from the panel it sits on. That is why light mode inverts the cap to machined charcoal on a bone chassis instead of pale-on-pale: measured keycap:panel is **6.81:1** in light and **9.55:1** in dark, where the pale-cap-on-pale-body reading collapsed to 1.16:1 (figure recorded in `index.css`).

**The AA Floor.** Every ink (`legend`, `legend-dim`, `legend-faint`, `amber-ink`, `red-ink`, `lamp-ok`) clears WCAG AA 4.5:1 against **all four** field colours (`chassis`, `panel`, `panel-raised`, `screen`) in **both** themes. Worst cases: **4.53** in dark (`legend-faint` on `panel-raised`) and **4.55** in light (`red-ink` on `screen`). The four code colours clear 4.5:1 on `screen` in both themes too. `clients/web/scripts/contrast.mjs` is the script that derives these — re-run it before shipping any token change.

## Typography

**Body Font:** Archivo Variable (with `system-ui`, `sans-serif`)
**Label / Mono Font:** Martian Mono Variable (with `ui-monospace`, `SF Mono`, `monospace`)

Both are self-hosted via `@fontsource-variable` and imported at the top of `index.css`. horsie servers routinely run on a LAN with no route to the public internet, so **no webfont may come from a CDN**. `font-synthesis-weight: none` is set globally, so only real variable weights render.

**Character:** Martian Mono is measurement, not costume — it names channels and reports values on an instrument face, and it is squeezed to `wdth 87.5` almost everywhere so a legend fits a control. Archivo carries anything a human reads as a sentence. There is no display face; the largest type in the build is a 15px page title. Scale comes from density and material, not from size.

### Hierarchy
- **Title** (Archivo, 600, 15px, `tracking-tight`): page and session titles in header bars. The top of the type scale.
- **Body** (Archivo, 400, 0.9375rem/1.65): agent prose, user messages, composer input, explanatory paragraphs. Prose is capped by the 54rem transcript column plus `max-w-prose` on standalone paragraphs.
- **UI** (Archivo, 400, 13px/1.25rem): session rows, nav items, popover options, task-list items, settings switch labels.
- **Legend** (`.legend` — Martian Mono, 500, 10px, `0.1em`, uppercase, `wdth 100`, `legend-faint`): the engraved panel label. Channel names ("TOKENS", "MODEL", "RUNTIME", "INPUT", "OUTPUT", "PLAN"), status words, timestamps, section captions, hints. The single most-used class in the build. It is the *only* legend at `wdth 100` — full width, because a label is read, not squeezed.
- **Readout** (`.readout` — Martian Mono, `tabular-nums`, `wdth 87.5`, `0.01em`, `amber-ink`): a live measured value. Token totals, task counts, context-window figures, runtime counts. Tabular so digits never jitter as they tick. Size is set at the call site (10–13px).
- **Key** (`.key` — Martian Mono, 550, 11px, `0.06em`, uppercase, `wdth 87.5`): every keycap legend.
- **Chip** (`.chip` — Martian Mono, 500, 10px, `0.04em`, `wdth 87.5`): engraved tags — versions, "Default", "not tested", ask-user choices.
- **Code** (Martian Mono, 11px/1.625, `wdth 87.5`): tool input/output, thinking text, `.field-mono`, prose `code` and `pre`. Section headings inside prose also use mono at 600 — a heading in an agent reply is machine structure, not editorial voice.

### Named Rules

**The Mono-Is-Measurement Rule.** Martian Mono is for legends, readouts, keycaps, and machine strings (model aliases, vendor names, repo names, paths, commands, IDs). Archivo is for anything that is a sentence. A machine string set in Archivo is a bug; a paragraph set in mono is a bug.

**The Tabular Readout Rule.** Any number that changes while you watch it uses `.readout`, not `text-amber`. It buys `tabular-nums` so the layout does not twitch, and it pairs with `animate-latch` so the value re-lights instead of popping.

## Layout

A three-column desk: **session rail** (17.5rem / `w-[17.5rem]`, `bg-panel`, right-ruled) — **content** (fluid, `min-w-0 flex-1`) — **task panel** (16rem / `w-64`, `bg-panel`, left-ruled, present only once the agent has used the task tool). Settings and Admin substitute their own 12rem (`w-48`) nav column, on `bg-chassis`, for the middle-left edge.

Content columns are capped, not fluid:
- **54rem** (`max-w-[54rem]`) — the transcript, composer, config bar, error banners and progression rows. Everything in a session shares one centred column so the recording reads as one strip.
- **48rem** (`max-w-3xl`) — settings and admin content, including the header bar's inner row, so a page title sits directly above its first panel rather than floating left of it.
- **`max-w-prose`** — standalone explanatory paragraphs.

Vertical rhythm inside the transcript is `gap-7` between turns and `space-y-2` between segments of one turn; panels are `p-4` with `space-y-2.5` rows; the header strip is `gap-x-5 gap-y-2` between readout channels. Every scrollable region gets the machined 10px scrollbar channel (`rule-strong` thumb, 1px radius, 3px transparent inset) — no rounded pill scrollbars.

**Breakpoints are Tailwind v4 defaults** (`sm` 40rem/640px, `md` 48rem/768px, `lg` 64rem/1024px); no custom breakpoints are declared. Three responsive rules carry the world:

- **Below `md`, the session rail becomes a drawer.** It goes `fixed inset-y-0 left-0`, slides on `translate-x`, gets `--panel-lift`, and is covered by a `oklch(0.1 0.01 255/0.6)` scrim. Pages render their own `<RailToggle/>` (a `.key-icon` hamburger, `md:hidden`) in their header so the control sits where the eye already is. The drawer closes on route change and on Escape. At 390px a persistent column would eat two thirds of the viewport.
- **Below `md`, the settings nav becomes a scrolling strip.** The column flips to a horizontal `overflow-x-auto` row of keys above the content, with a right-edge fade mask (`mask-image: linear-gradient(to right, black calc(100% - 2rem), transparent)`) so it reads as scrollable; the "SETTINGS" caption hides.
- **Below `lg`, the task panel overlays and starts collapsed.** It goes `absolute inset-y-0 right-0 z-20` over the transcript with `--panel-lift`, and initialises collapsed to a vertical `.key-icon` strip showing a `done/total` readout. Hiding it outright left narrow screens with a plan they could not ask for.
- **Below `sm`, the transcript gutter goes inline.** The 4.75rem right-aligned channel/timestamp column (`sm:w-[4.75rem] sm:flex-col sm:items-end`) becomes a single row above the turn's content.

## Elevation & Depth

Depth is material, not atmosphere. There are exactly **four** shadow tokens, they are theme-aware, and each does one of two jobs: push a surface **out** (a keycap, a floating panel) or cut it **in** (a screen, a ring). Nothing gets an ambient glow, and there is no elevation scale to climb.

### Shadow Vocabulary
- **Cap lift** (`--cap-lift`: `0 1px 0 var(--keycap-edge), 0 2px 4px oklch(0.1 0.01 255 / 0.55)`): the resting state of `.key`. The first layer is the cap's machined edge, the second is its drop.
- **Cap flat** (`--cap-flat`: `0 0 0 var(--keycap-edge), 0 1px 2px oklch(0.1 0.01 255 / 0.5)`): the pressed state. Applied with `translateY(1px)` — the cap travels and its edge collapses.
- **Panel lift** (`--panel-lift`: `0 1px 2px …/0.4, 0 4px 12px …/0.3`): anything that floats above the panel it belongs to — popovers, the display menu, the token breakdown, the mobile rail drawer, the overlaid task panel.
- **Screen inset** (`--screen-inset`: `inset 0 1px 3px oklch(0.08 0.01 255 / 0.75)`): `.screen` and `.field`. A recess, applied *first* so a focus ring can be appended after it.

Beyond these, depth is drawn with 1px inset rings rather than shadows: `shadow-[inset_0_0_0_1px_var(--rule)]` outlines a `.key-blank` and a settings row shell, `shadow-[inset_0_0_0_1px_var(--rule-strong)]` marks a selected nav row.

### Named Rules

**The Two-Direction Rule.** A shadow either lifts (cap, panel) or recesses (screen, ring). There is no third direction and no diffuse glow. Do not add a new shadow value — reuse one of the four tokens.

**The Ring-After-Recess Rule.** A focused `.field` composes `box-shadow: var(--screen-inset), 0 0 0 3px var(--focus-ring)`. Replacing the inset instead of appending to it flattens the field and breaks the recessed reading.

## Shapes

Four machined radii, and nothing between them: **3px** (`--radius-chip`) for chips, `kbd`, inline prose code, and small hit-target hover shells; **4px** (`--radius-control`) for fields, screens, icon keys, nav rows, banners, code blocks; **6px** (`--radius-cap`) for keycaps; **8px** (`--radius-panel`) for panels. Reach them as `var(--radius-*)` or the Tailwind `rounded-[var(--radius-control)]` form.

Three radii sit outside that scale, and each is reserved to exactly one part rather than being a step you may choose: the **lamp** is a 7px circle (`999px`) — the only pill in the system, because a real indicator lamp is round; `:focus-visible` normalises to **2px** so the amber outline traces a control tightly regardless of what it wraps; and the **scrollbar thumb** is **1px**, because the channel is machined, not pill-shaped. They are recorded in the `rounded` scale as `lamp`, `focus` and `scrollbar` so the system is auditable, not so they are available.

Borders are hairline and uniform: `* { border-color: var(--rule) }` is set globally, so `border`, `border-b`, `border-l` need no colour class. Weight is carried by `rule-strong`, never by thickness — except `.kbd`, whose 2px bottom border is the one place a border models a physical edge.

## Components

The vocabulary is defined once in `clients/web/src/index.css` as Tailwind `@utility` (`panel`, `screen`, `legend`, `readout`) and `@layer components` classes (everything else). Build new surfaces out of these; do not restyle them locally.

### Keys (`.key` and variants)
Everything you press is a key: it has travel, an edge, and a legend.
- **Shape:** machined cap corner (6px), padding `8px 14px`, `inline-flex` with `gap-0.5rem` for an icon.
- **Default:** keycap material with keycap ink and `--cap-lift`. Hover raises to `keycap-hover`; `:active` translates 1px down and swaps to `--cap-flat`; `:disabled` drops to `opacity: 0.38` and `pointer-events: none`.
- **`.key-go`** — the one control that commits: safety orange on orange ink. Used for Send, Save, Add, Create, New agent, and answering an ask.
- **`.key-stop`** — interrupt and destroy: red, hover `brightness(1.08)`. Used for stopping a turn in the composer and in the session header.
- **`.key-go` / `.key-stop` focus:** these two override the global amber ring with `outline-color: var(--orange-ink)` at `outline-offset: 3px`. Amber on safety orange is barely a ring, so the highest-stakes controls focus in their own ink.
- **`.key-blank`** — an unpressed area of the panel that is still a control: transparent with a 1px inset rule, hover fills to `panel-raised`. The secondary action beside a `.key-go` (e.g. Discard).
- **`.key-flat`** — bare legend text that responds: transparent, no shadow, no travel. Menu rows and inline toggles.
- **`.key-danger`** — transparent with red ink, hover washes `red-quiet`. Defined and reserved for a destructive text action; currently unused in the app.
- **`.key-icon`** — a 2rem square icon key at 4px radius, transparent, hover fills `panel-raised`. Rail toggle, theme toggle, display menu, delete, panel collapse. Delete buttons override hover to `red-quiet`/`red-ink`.

### Fields (`.field`, `.field-mono`)
- **Style:** a slot cut into the panel — `screen` background, 1px rule, 4px radius, `--screen-inset`, `8px 11px` padding, Archivo 0.875rem. Placeholder is `legend-faint`.
- **Focus:** border shifts to `amber` and a 3px `--focus-ring` is appended after the inset.
- **`.field-mono`:** the same slot at 0.8125rem in Martian Mono — for identifiers, endpoints, model names, URLs.
- **`select.field`** ships its own two-gradient chevron so no browser control leaks through; `input[type=checkbox|radio]` are given `accent-color: var(--orange)` at 0.9375rem for the same reason.
- **Disabled:** `opacity: 0.5`.

### Panels and Screens (`.panel`, `.screen`)
- **`.panel`** — `panel` background, 1px rule, 8px radius. Cards, popovers, the composer shell, settings sections. Floating instances add `shadow-[var(--panel-lift)]`.
- **`.screen`** — `screen` background, 1px rule, 4px radius, `--screen-inset`. Wraps machine output only: tool input/output `<pre>`, thinking text, prose `<pre>`, the context meter track, empty-state wells.

### Chips and Keycaps of Text (`.chip`, `.kbd`)
- **`.chip`** — `panel-raised` with a 1px rule at 3px radius, mono 10px, `legend-dim`. Versions, "Default", "not tested", and ask-user choices (which add `border-amber bg-amber/15` when selected).
- **`.kbd`** — the same material with a 2px bottom border and 9px mono `legend-faint`. Defined for keyboard hints; currently unused.

### Lamps (`.lamp`, `.lamp-live`, `.lamp-off`)
The signature component. A 7px dot filled with `currentColor` and a 6px glow of the same colour, so a lamp's tone is set by putting a text colour on it (`text-amber-ink`, `text-lamp-ok`, `text-red-ink`, `text-orange`, `text-faint`).
- **`.lamp-live`** adds a 1px ring that pings out to `scale(2.1)` over 1.6s — live work only.
- **`.lamp-off`** hollows the dot to a 1.5px inset ring at `opacity: 0.55` — a channel the server has nothing to report for.
- Status tones map through `TONE_TEXT` in `src/lib/status.ts`: `live` → amber, `ready` → green, `attention` → orange, `fault` → red, `off` → faint. Add a status by adding a tone there, not by hard-coding a colour.

### Navigation
- **Session rail:** a nameplate (orange `h` cap, `HORSIE` in mono at `0.16em`, plus the feed's own lamp and word), a mono search field beside a `.key`, a legend-captioned count, then one row per session — lamp, 13px title, and a legend line of "status · relative time". Active rows are `bg-raised text-legend` with a `rule-strong` inset ring; inactive are `text-dim` hovering to `bg-raised text-legend`. The footer carries three 10px mono links (Agents / Settings / Admin) and the theme toggle.
- **Settings nav:** the same active/inactive treatment at 13px with a 14px icon, on `bg-chassis` so the nav column reads as chassis behind the panel content.

### Signature: the header strip
A session header is an instrument face, not a toolbar. Row one is title + `StatusBadge` (lamp and word) + a "Reconnecting" lamp when the feed drops + the display menu, Stop, and Delete. Row two is a row of legend-over-value channels — `TOKENS` with its amber readout, then `MODEL`, `RUNTIME`, and the rest of the locked session config as engraved labels, deliberately not styled as buttons, because a settled channel is a description of the session and not a control.

### Signature: the transcript
One stamped gutter down the left (`Agent` / `You` plus the server's own timestamp, never a local clock), and content to its right. Consecutive assistant messages collapse into one entry. Multi-step work collapses into a single legend row — "Ran 2 tools", "Thought and ran 3 tools" — carrying a live lamp while running and a duration when finished; expanding reveals the ordered list against a left rule. Tool calls collapse to one line (chevron, state icon, mono name, truncated input preview) and expand to `INPUT` and `OUTPUT` on recessed screens. Thinking blocks are a legend row that expands to faint mono on a screen.

### Motion
One authored gesture: things arrive by settling into the panel, and values cross-fade the way an amber display re-latches.
- **`.animate-settle`** (`220ms cubic-bezier(0.16, 1, 0.3, 1)`, `opacity 0 → 1`, `translateY(4px) → 0`): a transcript turn arriving.
- **`.animate-latch`** (`260ms ease-out`, `opacity 0.25 → 1`, re-keyed on the value): a readout changing.
- **`.caret`** (`0.55em × 1.05em` amber block with an 8px glow, `1.1s steps(1)` between `opacity: 1` and `0.15`): the streaming cursor — a lit segment on the screen, not a blinking bar.
- **`lamp-ping`** (`1.6s cubic-bezier(0, 0, 0.2, 1)` infinite): the only looping animation, and only on `.lamp-live`.
- **Key travel:** `--cap-lift` → `--cap-flat` with `translateY(1px)` over 90ms; background over 120ms.
- **Reduced motion:** `@media (prefers-reduced-motion: reduce)` clamps every animation and transition to `0.01ms` globally. Any new motion must ride the existing keyframes or the clamp will not cover the intent.

## Do's and Don'ts

### Do:
- **Do** build from the existing classes — `.key`/`.field`/`.panel`/`.screen`/`.legend`/`.readout`/`.chip`/`.lamp`. A new surface should add layout, not new materials.
- **Do** reach colour through the semantic Tailwind aliases (`bg-panel`, `bg-raised`, `text-dim`, `text-amber-ink`, `border-rule`). Raw hex and Tailwind's stock palette are both out.
- **Do** pair every lamp with a word — visible, or `sr-only` when the visual pair is an icon. Follow the `TONE_TEXT` map for status colour.
- **Do** put machine strings (model aliases, vendor names, paths, commands, IDs) in Martian Mono, and sentences in Archivo.
- **Do** use `.readout` for any number that ticks, and `.legend` for the word that names it. Legend above value, mono both.
- **Do** put raw machine output on a `.screen`. Tool input, tool output, thinking, code — recessed, never a floating card.
- **Do** re-run `clients/web/scripts/contrast.mjs` after touching any colour token; every ink must stay ≥ 4.5:1 against all four field colours in both themes.
- **Do** define new colours in both `:root` and `[data-theme="light"]`, and pick light values by *role*, not by lightening the dark value.
- **Do** give a new floating surface `shadow-[var(--panel-lift)]` and a new recessed one `var(--screen-inset)`.
- **Do** give any new column a below-`md` (or below-`lg`, for a third column) collapse — drawer, strip, or overlay — matching the rail, the settings nav, and the task panel.

### Don't:
- **Don't** put a second `.key-go` on a screen. One orange control commits; secondary is `.key-blank`, inline is `.key-flat`, destructive is `.key-stop` or `.key-danger`.
- **Don't** use amber for emphasis or red for importance. Amber means *measured and live*; red means *interrupt or destroy*.
- **Don't** drive syntax highlighting, charts, or any decorative colour from `--orange` or `--amber`. Code has its own `--code-*` palette; anything else needs a new semantic token with a stated meaning.
- **Don't** convey state with colour alone — no bare coloured dot, no red text with no word, no "the button turns green".
- **Don't** make a light-theme keycap pale. A keycap is a different material from its panel in both themes; pale-on-pale measured 1.16:1 and destroyed the signature separation.
- **Don't** introduce a new radius. 3 / 4 / 6 / 8px are the scale; `lamp` (999px), `focus` (2px) and `scrollbar` (1px) are reserved to those three parts and nothing else. No pills, no `rounded-full` on a control.
- **Don't** add a fifth shadow, a glow, or an elevation scale. Lift or recess, four tokens.
- **Don't** import a font from a CDN or add a third family. horsie servers run on LANs with no internet route; both faces are self-hosted through `@fontsource-variable`.
- **Don't** replace `--screen-inset` when adding a focus ring — append to it.
- **Don't** set a transcript-width container to anything but `max-w-[54rem]`, or a settings-width one to anything but `max-w-3xl`. Two column widths, consistently applied, are what make the app read as one instrument.
