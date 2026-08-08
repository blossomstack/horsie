# Routine schedule settings UX

- **Date:** 2026-08-07
- **Status:** Approved design
- **Scope:** Routine creation and editing in `clients/web/src/pages/routines/RoutineEditPage.tsx`

## Problem

Weekly day selection currently renders each day as a standalone chip. The selected state uses the safety-orange commit color, so selected days compete with the Save action and do not read as one coherent day-of-week control. Calendar schedules also expose the full timezone picker beside the time on every visit, even though most users want the browser's local timezone and rarely need to change it.

## Goals

1. Make weekly day selection read as one grouped control with an unmistakable selected state.
2. Keep the browser's IANA timezone as the default for new calendar schedules.
3. Hide timezone editing until the user explicitly asks to change it.
4. Preserve existing schedule payloads, server ordering, edit behavior, and the current horsie Console visual system.
5. Keep the controls keyboard accessible and usable on narrow screens.

## Non-goals

- No changes to the routine API or generated wire types.
- No calendar date-navigation widget; weekly recurrence does not require date navigation.
- No new color tokens or global component abstractions.
- No change to how one-time schedules interpret browser-local datetime input.

## Design

### Weekly days

Render Mon–Sun inside an explicitly named `role="group"` with a label such as `Days`. Each day remains a native `button` with `aria-pressed`, but receives an equal minimum width and touch-friendly minimum height so the row reads as a compact segmented selector rather than seven unrelated tags. Keep the existing canonical Mon–Sun order and `Set<Weekday>` state.

- Unselected days use the neutral control material and retain a clear hover/focus state.
- Selected days use the semantic amber selection treatment already used for selected choices: an amber border/inset ring, a light amber wash, and amber text. Safety orange remains reserved for the save/commit action.
- Give each abbreviated visual label an accessible full-day name.
- Move the existing preset out of the day row and label it `Weekdays`; style it as a secondary inline action so it cannot be mistaken for an eighth day.
- Keep the existing `data-testid="weekday-*"` hooks and add a stable test id for the preset/group only where useful.

### Timezone disclosure

Keep `browserTimezone()` as the initial timezone for new calendar schedules. For edits, seed from the stored schedule exactly as today. Add local disclosure state that starts closed on both new and edit forms.

The closed state shows a compact summary next to the time:

- `Browser timezone · <IANA zone>` when the selected zone matches the browser zone.
- `Custom timezone · <IANA zone>` when it does not.

A native button labeled `Change` toggles the full timezone select. It exposes `aria-expanded` and `aria-controls`, and the select keeps its existing `data-testid="routine-timezone-select"`. The selected timezone is never reset when the editor is opened or closed. A custom stored timezone remains visible in the summary but does not force the editor open.

## Implementation boundaries

- Keep all changes local to the routine edit form and its focused unit tests.
- Use existing Console utility classes and semantic tokens (`border-amber`, `bg-amber/15`, `text-amber-ink`, `bg-raised`, `field`, `key-flat`); do not add raw colors, new radii, or a new CSS component.
- Keep the current `buildSchedule` function and validation rules unchanged except for presentation state.

## Interaction and accessibility

- Day buttons remain native buttons and retain `aria-pressed` so keyboard users and assistive technology receive the selected state.
- The day group has an accessible name and each abbreviated button has a full-day accessible name.
- The timezone disclosure is a native button with a visible focus state, `aria-expanded`, and an explicit relationship to the revealed select.
- The Save button remains the only safety-orange commit control in this form.
- The day selector wraps rather than causing horizontal overflow at narrow widths.

## Testing and verification

Update `RoutineEditPage.test.tsx` to verify:

1. A new daily schedule defaults to the browser timezone while the timezone select is initially absent/hidden.
2. Activating `Change` reveals the existing timezone select and exposes the correct expanded state.
3. A weekly schedule starts with no selected days, selected days expose the selected treatment and `aria-pressed`, toggling works, and the saved payload remains canonical.
4. The `Weekdays` preset selects Mon–Fri without changing the payload shape.

Run the focused Vitest file, the routine Playwright spec when its harness is available, the web build/typecheck, and the Impeccable detector once over the changed UI files. Since no CSS tokens change, the contrast script is not required unless the implementation introduces a token change.
