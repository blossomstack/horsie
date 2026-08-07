# Setting names in new-session popups

- **Date:** 2026-08-06
- **Status:** Approved design

## Problem

The compact setting controls on the new-session screen open icon-style popups. Those popups show the options but do not show the setting name. After a session exists, the equivalent readout popups include the name at the top. This makes the same settings feel different before and after session creation.

## Design

Use the existing `PickerSpec.legend` as the visible heading for every icon-style `PopoverMenu` popup. The heading will render above the existing picker or readout content, using the popup's existing visual language. This applies to every setting exposed by the shared picker system: Workflow, Runtime, Repos, Skills, MCP, Memory, Model, and Thinking, subject to the setting being available for the current session draft.

The heading responsibility will live in `PopoverMenu`, rather than in draft-only wrappers. Locked-session readouts currently render their own heading inside the readout body, so that duplicate heading will be removed from the readout helper. Both draft pickers and locked readouts will then use the same popup-level heading and remain visually consistent.

Existing trigger labels, accessible names, option values, selection behavior, popup placement, and field-style controls will not change. The existing `legend` remains the source for the trigger tooltip and accessible name.

## Components and data flow

`useConfigPickers` and `useLockedChannels` already supply a `legend` for each setting. `SessionConfigBar` passes that value to `PopoverMenu` in both draft and locked modes. `PopoverMenu` will render the legend only for the icon-style popup content; field-style controls already render the legend on their trigger and should not gain a popup heading through this change. The readout body will continue to render only its values.

## Testing

Add component-level coverage for the popup content to verify that opening representative new-session setting controls exposes the expected visible setting name, including Model and Thinking. Verify that the locked-session readout still contains each setting name exactly once after the heading is centralized. Existing tests for accessible names, selected values, and omitted optional channels remain valid.

## Scope and error handling

This is presentation-only. No API, persistence, or state behavior changes are needed, and there are no new error paths. Settings that are not available remain omitted exactly as they are today.
