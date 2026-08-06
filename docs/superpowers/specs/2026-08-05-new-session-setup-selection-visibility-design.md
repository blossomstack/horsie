# New-session setup selection visibility

## Goal

Make the selected values in the existing new-session setup pickers immediately recognizable when a user opens a picker to review the setup. This is especially important for non-checkbox choices such as runtime and model, whose current options are otherwise only implied by the small configuration key.

## Design

Keep the current setup row, picker menus, option ordering, and interaction model. Add selected-state styling to the option that matches the current draft value:

- Runtime: highlight the selected vendor and show a checkmark.
- Model: highlight the selected model alias and show a checkmark.
- Workflow: highlight the selected workflow, including the `None` choice when no workflow is selected.
- Thinking effort: retain its existing radio controls; add the same selected-row treatment so the selected effort is clear without relying on the native radio alone.

The selected state should use the existing visual language for raised/selected menu rows, and options should expose selection semantics (`aria-pressed` where appropriate, or the existing native radio semantics for thinking effort). Checkbox lists and their checked states remain unchanged.

Apply the styling to the existing draft pickers only; no new summary, labels, controls, or duplicated setup information are introduced. The locked session readout remains unchanged because it already represents frozen values rather than editable options.

## Implementation boundaries

- Extend the shared picker option styling rather than creating a second picker implementation.
- Pass the current value into option rendering only where needed to determine selection.
- Keep selecting an option, closing menus, persistence, and request construction unchanged.

## Verification

Add focused component tests that render the picker menus and assert the selected option has the selected-state marker/class while an unselected option does not. Preserve existing tests for draft values, checkbox behavior, and thinking radio selection. Run the focused web tests and formatting/type checks, followed by the repository-required verification available in the workspace.
