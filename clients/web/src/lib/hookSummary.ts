import type { HookRecord } from "../api/types";
import { i18n } from "../i18n";

/** What a hook did, in a few words, plus whether it changed anything.
 *
 * The record is the audit trail; this is the line a person reads without
 * unpacking it, and `intervened` is what the collapsed tool row and the notice
 * row's styling both key off. */
export interface HookSummary {
  text: string;
  intervened: boolean;
}

// Functions, not constants: a module-level constant is built once at import,
// so its wording would stay in whatever language the tab was opened in.
const allowed = (): HookSummary => ({
  text: i18n.t("hook.allowed"),
  intervened: false,
});
const ran = (): HookSummary => ({ text: i18n.t("hook.ran"), intervened: false });

function failed(reason: string): HookSummary {
  return { text: i18n.t("hook.couldNotRun", { reason }), intervened: true };
}

function rewroteInput(): HookSummary {
  return { text: i18n.t("hook.rewroteInput"), intervened: true };
}

function rewroteOutput(): HookSummary {
  return { text: i18n.t("hook.rewroteOutput"), intervened: true };
}

/** The call a record guarded, or `null` when it guarded none.
 *
 * The split every rendering decision hangs off: a record with no call cannot
 * attach to a tool card, so it becomes a transcript row of its own. */
export function toolScope(
  r: HookRecord,
): { tool: string; toolCallId: string } | null {
  const a = r.action;
  switch (a.event) {
    case "PreToolUse":
    case "PostToolUse":
    case "PostToolUseFailure":
      return a.value.call;
    // A batch names every call it covered, so no single one owns it.
    case "PostToolBatch":
    case "SessionStart":
    case "SessionEnd":
    case "UserPromptSubmit":
    case "UserPromptExpansion":
    case "Stop":
    case "StopFailure":
    case "SubagentStart":
    case "SubagentStop":
    case "TaskCreated":
    case "TaskCompleted":
    case "Notification":
    case "CwdChanged":
    // A compaction is not a tool call, so neither attaches to a tool card.
    case "PreCompact":
    case "PostCompact":
      return null;
  }
}

/** The text a hook addressed to the *user*, never to the model.
 *
 * The four side-effect-only events permit no JSON output at all, so they carry
 * no such field — hence the narrowing rather than a blind property read. */
export function systemMessage(r: HookRecord): string | null {
  const a = r.action;
  switch (a.event) {
    case "SessionEnd":
    case "StopFailure":
    case "Notification":
    case "CwdChanged":
    // Side-effect only, like the four above: by the time it runs the boundary
    // exists and there is nothing left to say about it.
    case "PostCompact":
      return null;
    default:
      return a.value.systemMessage ?? null;
  }
}

/** Whether this hook stopped the call from running as the agent asked.
 *
 * Narrower than `intervened`: a hook that rewrote an input or an output also
 * intervened, but the call still ran. Only `PreToolUse` can stop one — it is
 * the only event that runs before the tool does, and the only one that fails
 * closed — so a `PostToolUse` objection is detail rather than state.
 */
export function deniesCall(r: HookRecord): boolean {
  if (r.action.event !== "PreToolUse") return false;
  const o = r.action.value.outcome.outcome;
  return o === "Denied" || o === "Failed";
}

/** What a hook did, including whether it stopped horsie afterwards.
 *
 * A halt is read off the envelope rather than the outcome, because `continue`
 * is a common field: any hook on any event may set it, and one that both
 * allowed its call and halted the turn has done two separate things. It is
 * reported first, since it is the larger of the two. */
export function hookSummary(r: HookRecord): HookSummary {
  const outcome = outcomeSummary(r);
  if (!r.halt) return outcome;
  const why = r.halt.reason ?? i18n.t("hook.noReason");
  return {
    text: i18n.t("hook.stoppedHorsie", { why, outcome: outcome.text }),
    intervened: true,
  };
}

/** No `default` clause in the switch below, deliberately: adding a `HookAction`
 * arm must fail `tsc` rather than fall through to a generic sentence. That is
 * the TypeScript half of the guarantee the Rust union gives. */
function outcomeSummary(r: HookRecord): HookSummary {
  const a = r.action;
  switch (a.event) {
    case "PreToolUse":
      switch (a.value.outcome.outcome) {
        case "Allowed":
          return a.value.outcome.value.input ? rewroteInput() : allowed();
        case "Denied":
          return {
            text: a.value.outcome.value.reason ?? i18n.t("hook.deniedCall"),
            intervened: true,
          };
        // horsie has no permission prompt, so there is nobody to ask.
        case "Ask":
        case "Defer":
          return { text: i18n.t("hook.askedApproval"), intervened: false };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "PostToolUse":
      switch (a.value.outcome.outcome) {
        case "Ran": {
          const result = a.value.outcome.value;
          if (result.output) return rewroteOutput();
          if (result.additionalContext)
            return { text: i18n.t("hook.addedResultContext"), intervened: true };
          return allowed();
        }
        case "Blocked":
          return {
            text:
              a.value.outcome.value.reason ??
              i18n.t("hook.objectedAlreadyRan"),
            intervened: true,
          };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "PostToolUseFailure":
    case "PostToolBatch":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return a.value.outcome.value.additionalContext
            ? { text: i18n.t("hook.addedContext"), intervened: true }
            : ran();
        case "Blocked":
          return {
            text: a.value.outcome.value.reason ?? i18n.t("hook.objected"),
            intervened: true,
          };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "SessionStart":
    case "SubagentStart":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return a.value.outcome.value.additionalContext
            ? { text: i18n.t("hook.addedSessionContext"), intervened: true }
            : ran();
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "UserPromptSubmit":
    case "UserPromptExpansion":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return a.value.outcome.value.additionalContext
            ? { text: i18n.t("hook.addedPromptContext"), intervened: true }
            : ran();
        case "Blocked":
          return {
            text: a.value.outcome.value.reason ?? i18n.t("hook.rejectedPrompt"),
            intervened: true,
          };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    // Both stop events read the same way: a block is a block *from stopping*,
    // which is the opposite of a refusal. Separate arms because their outcome
    // unions are separate types, and folding `SubagentStop` in with the
    // objection-shaped events silently dropped its `CapReached` on the floor.
    case "Stop":
    case "SubagentStop":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return a.value.outcome.value.additionalContext
            ? { text: i18n.t("hook.leftNote"), intervened: true }
            : ran();
        case "Blocked":
          return {
            text: i18n.t("hook.keptTurnGoing", {
              reason: a.value.outcome.value.reason ?? i18n.t("hook.noReason"),
            }),
            intervened: true,
          };
        case "CapReached":
          return {
            text: i18n.t("hook.hitContinuationLimit", {
              reason: a.value.outcome.value.reason ?? i18n.t("hook.noReason"),
            }),
            intervened: true,
          };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "PreCompact":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return ran();
        case "Blocked":
          return {
            text: i18n.t("hook.stoppedCompaction", {
              reason: a.value.outcome.value.reason ?? i18n.t("hook.noReason"),
            }),
            intervened: true,
          };
        // A compaction has no continuation budget to exhaust: a block abandons
        // it once and nothing loops. The arm exists because the outcome type is
        // shared with `Stop`, which does.
        case "CapReached":
          return {
            text: i18n.t("hook.stoppedCompaction", {
              reason: a.value.outcome.value.reason ?? i18n.t("hook.noReason"),
            }),
            intervened: true,
          };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "TaskCreated":
    case "TaskCompleted":
    case "SessionEnd":
    case "StopFailure":
    case "Notification":
    case "PostCompact":
    case "CwdChanged":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return ran();
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
  }
}
