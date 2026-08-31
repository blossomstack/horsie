import type { ReactNode } from "react";
import type { AgentDocument, SessionDetail } from "../api/types";
import type { ConfigDraft } from "../hooks/useSessionDraft";
import { useFrozenDraft } from "../hooks/useSessionDraft";
import type { PickerSpec } from "./configPickers";
import { useConfigPickers } from "./configPickers";
import { PopoverMenu } from "./PopoverMenu";

type Props =
  | { mode: "draft"; draft: ConfigDraft }
  | { mode: "locked"; detail: SessionDetail; agent: AgentDocument };

/** The row of channel keys, in the one place it lives on both surfaces. */
function KeyRow({
  children,
  mode,
}: {
  children: ReactNode;
  mode: "draft" | "locked";
}) {
  return (
    <div className="mx-auto w-full max-w-[54rem] px-4 sm:px-6">
      <div
        className="flex items-center gap-0.5 pb-1.5 pt-1.5"
        data-testid="session-config-bar"
        data-mode={mode}
      >
        {children}
      </div>
    </div>
  );
}

/**
 * The session's channels, above the composer.
 *
 * The same row before and after the session exists: same keys, same order,
 * same place. Creating a session freezes the values but does not move the
 * control that shows them — before, the launched-with facts jumped from above
 * the input to a popover in the header, so the one row you had just used to
 * configure the session was somewhere else the moment you sent a message.
 */
export function SessionConfigBar(props: Props) {
  if (props.mode === "locked") {
    return <LockedRow detail={props.detail} agent={props.agent} />;
  }
  return <DraftRow draft={props.draft} />;
}

function DraftRow({ draft }: { draft: ConfigDraft }) {
  const pickers = useConfigPickers(draft);
  return (
    <KeyRow mode="draft">
      <KeyControls pickers={pickers} />
    </KeyRow>
  );
}

/**
 * The same row for a session that already exists.
 *
 * One hook, one mode flag — not a second set of controls. The session's frozen
 * values are dressed as a draft nothing can write to, so every key, its order,
 * and the list inside it are literally the same code as the new-session row.
 */
function LockedRow({
  detail,
  agent,
}: {
  detail: SessionDetail;
  agent: AgentDocument;
}) {
  const frozen = useFrozenDraft(detail, agent);
  const pickers = useConfigPickers(frozen, "frozen");
  return (
    <KeyRow mode="locked">
      <KeyControls pickers={pickers} />
    </KeyRow>
  );
}

/**
 * The action row's pickers, as bare keys.
 *
 * Every control is an icon with a dot when it holds a value; the channel name
 * and the current value both live in the tooltip and the accessible name, so
 * nothing is lost to a screen reader or to a hover. Model and Thinking sit
 * last and adjacent — effort is a property of the model, so they read as one
 * decision.
 */
function KeyControls({ pickers }: { pickers: PickerSpec[] }) {
  return (
    <>
      {pickers.map((p) => (
        <PopoverMenu
          key={p.key}
          // Model and Thinking sit apart from the workspace channels: effort
          // is a property of the model, and the pair is the decision you
          // revisit most.
          className={p.key === "model" ? "ml-auto" : undefined}
          variant="icon"
          placement="up"
          testId={p.testId}
          legend={p.legend}
          icon={p.icon}
          label={p.label}
          marked={p.marked}
          warn={p.warn}
          width={p.width}
          height={p.height}
        >
          {p.body}
        </PopoverMenu>
      ))}
    </>
  );
}

/**
 * The same pickers as labelled form rows.
 *
 * The agent editor used to render the action row verbatim, which dragged its
 * bottom-anchored layout into a form and left the configuration floating below
 * the fields it belongs with.
 */
export function ConfigFields({ draft }: { draft: ConfigDraft }) {
  const pickers = useConfigPickers(draft);
  return (
    <div
      className="grid grid-cols-1 gap-3 sm:grid-cols-2"
      data-testid="config-fields"
    >
      {pickers.map((p) => (
        <div key={p.key} className="min-w-0">
          <PopoverMenu
            variant="field"
            placement="down"
            testId={p.testId}
            legend={p.legend}
            icon={p.icon}
            label={p.label}
            width={p.width}
            height={p.height}
          >
            {p.body}
          </PopoverMenu>
        </div>
      ))}
    </div>
  );
}
