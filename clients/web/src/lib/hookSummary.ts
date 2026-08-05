import type { HookRecord } from "../api/types";

/** What a hook did, in a few words, plus whether it changed anything.
 *
 * The record is the audit trail; this is the line a person reads without
 * unpacking it, and `intervened` is what the collapsed tool row and the notice
 * row's styling both key off. */
export interface HookSummary {
  text: string;
  intervened: boolean;
}

const ALLOWED: HookSummary = { text: "allowed", intervened: false };
const RAN: HookSummary = { text: "ran", intervened: false };

function failed(reason: string): HookSummary {
  return { text: `could not run — ${reason}`, intervened: true };
}

function rewrote(what: string): HookSummary {
  return { text: `rewrote the ${what}`, intervened: true };
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
    case "Stop":
    case "StopFailure":
    case "SubagentStart":
    case "SubagentStop":
    case "TaskCreated":
    case "TaskCompleted":
    case "Notification":
    case "CwdChanged":
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

/** No `default` clause in the switch below, deliberately: adding a `HookAction`
 * arm must fail `tsc` rather than fall through to a generic sentence. That is
 * the TypeScript half of the guarantee the Rust union gives. */
export function hookSummary(r: HookRecord): HookSummary {
  const a = r.action;
  switch (a.event) {
    case "PreToolUse":
      switch (a.value.outcome.outcome) {
        case "Allowed":
          return a.value.outcome.value.input ? rewrote("input") : ALLOWED;
        case "Denied":
          return {
            text: a.value.outcome.value.reason ?? "denied the call",
            intervened: true,
          };
        // horsie has no permission prompt, so there is nobody to ask.
        case "Ask":
        case "Defer":
          return { text: "asked for approval — allowed", intervened: false };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "PostToolUse":
      switch (a.value.outcome.outcome) {
        case "Ran": {
          const ran = a.value.outcome.value;
          if (ran.output) return rewrote("output");
          if (ran.additionalContext)
            return { text: "added context to the result", intervened: true };
          return ALLOWED;
        }
        case "Blocked":
          return {
            text:
              a.value.outcome.value.reason ??
              "objected — the call had already run",
            intervened: true,
          };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "PostToolUseFailure":
    case "PostToolBatch":
    case "SubagentStop":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return a.value.outcome.value.additionalContext
            ? { text: "added context", intervened: true }
            : RAN;
        case "Blocked":
          return {
            text: a.value.outcome.value.reason ?? "objected",
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
            ? { text: "added session context", intervened: true }
            : RAN;
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "UserPromptSubmit":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return a.value.outcome.value.additionalContext
            ? { text: "added context to the prompt", intervened: true }
            : RAN;
        case "Blocked":
          return {
            text: a.value.outcome.value.reason ?? "rejected the prompt",
            intervened: true,
          };
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
    case "Stop":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return a.value.outcome.value.additionalContext
            ? { text: "left a note for the next turn", intervened: true }
            : RAN;
        // Blocked means blocked *from stopping*: the opposite of a refusal.
        case "Blocked":
          return {
            text: `kept the turn going — ${a.value.outcome.value.reason ?? "no reason given"}`,
            intervened: true,
          };
        case "CapReached":
          return {
            text: `hit the continuation limit — ${a.value.outcome.value.reason ?? "no reason given"}`,
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
    case "CwdChanged":
      switch (a.value.outcome.outcome) {
        case "Ran":
          return RAN;
        case "Failed":
          return failed(a.value.outcome.value.reason);
      }
  }
}
