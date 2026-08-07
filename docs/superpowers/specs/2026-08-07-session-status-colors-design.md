# Session Status Color Hierarchy Design

- **Date:** 2026-08-07
- **Scope:** Web client session status presentation

## Goal

Make the session status hierarchy communicate activity clearly: a running session should be visually prominent and animated, while an idle session should remain visible without competing with the rest of the theme.

## Design

Keep `Running` on the existing semantic `live` tone. It will continue to use the amber text color and `busy: true`, so the status label and animated lamp share the same running color.

Introduce a distinct semantic `idle` tone for `Idle`. Map that tone to the existing `text-dim` utility rather than adding new theme tokens. Idle remains non-animated and its label, hint, sendability, and all other behavior stay unchanged.

The existing `off` tone remains reserved for an unknown/unloaded status. Other statuses retain their current tones and behavior.

## Data flow

`statusMeta()` continues to translate each `SessionStatusKind` to metadata. `StatusBadge` and `StatusDot` consume the metadata without changes: `TONE_TEXT` supplies the text color, and `busy` controls the live lamp animation.

## Testing

Add or update unit coverage for the status metadata to verify:

- Idle resolves to the new `idle` tone and is not busy.
- Running resolves to `live` and remains busy.
- The `idle` tone resolves to `text-dim`.
- Existing unknown/off behavior remains distinct.

No visual redesign, status-label changes, or changes to animation timing are in scope.
