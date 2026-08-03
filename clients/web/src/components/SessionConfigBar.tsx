import type { ReactNode } from "react";
import type { SessionDetail } from "../api/types";
import type { ConfigDraft } from "../hooks/useSessionDraft";
import { basename } from "../lib/format";
import { useConfigPickers } from "./configPickers";
import { PopoverMenu } from "./PopoverMenu";

type Props =
  | { mode: "draft"; draft: ConfigDraft }
  | { mode: "locked"; detail: SessionDetail };

/** A settled channel on the header strip: engraved legend over its value.
 * Locked config is a description of the session, not a control, so it reads
 * as an instrument label rather than a button you might be able to press. */
function Readout({
  legend,
  children,
  testId,
}: {
  legend: string;
  children: ReactNode;
  testId?: string;
}) {
  return (
    <span className="flex min-w-0 flex-col gap-0.5" data-testid={testId}>
      <span className="legend leading-none">{legend}</span>
      <span className="font-mono text-[11px] leading-snug break-words text-legend">
        {children}
      </span>
    </span>
  );
}

export function SessionConfigBar(props: Props) {
  if (props.mode === "locked") {
    // A stacked list, not a strip: locked config now lives inside a narrow
    // info popover rather than spanning the header.
    return (
      <div
        className="space-y-2.5"
        data-testid="session-config-bar"
        data-mode="locked"
      >
        <LockedControls detail={props.detail} />
      </div>
    );
  }
  return (
    <div className="mx-auto w-full max-w-[54rem] px-4 sm:px-6">
      <div
        className="flex items-center justify-end gap-0.5 pb-1.5"
        data-testid="session-config-bar"
        data-mode="draft"
      >
        <DraftControls draft={props.draft} />
      </div>
    </div>
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
function DraftControls({ draft }: { draft: ConfigDraft }) {
  const pickers = useConfigPickers(draft);
  return (
    <>
      {pickers.map((p) => (
        <PopoverMenu
          key={p.key}
          variant="icon"
          placement="up"
          testId={p.testId}
          legend={p.legend}
          icon={p.icon}
          label={p.label}
          marked={p.marked}
          warn={p.warn}
          width={p.width}
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
          >
            {p.body}
          </PopoverMenu>
        </div>
      ))}
    </div>
  );
}

function LockedControls({ detail }: { detail: SessionDetail }) {
  return (
    <>
      <Readout legend="Model" testId="config-model">
        {detail.model}
      </Readout>
      <Readout legend="Runtime" testId="config-runtime">
        {detail.vendor}
      </Readout>
      {detail.repos.length > 0 && (
        <Readout legend="Repos" testId="config-repos">
          {detail.repos.map((r) => basename(r)).join(", ")}
        </Readout>
      )}
      {detail.plugins.length > 0 && (
        <Readout legend="Skills" testId="config-skills">
          {detail.plugins.join(", ")}
        </Readout>
      )}
      {detail.mcpServers.length > 0 && (
        <Readout legend="MCP" testId="config-mcp">
          {detail.mcpServers.join(", ")}
        </Readout>
      )}
      {detail.memorySpaces.length > 0 && (
        <Readout legend="Memory" testId="config-memory">
          {detail.memorySpaces.join(", ")}
        </Readout>
      )}
      {detail.thinkingEffort && (
        <Readout legend="Thinking" testId="config-thinking">
          {detail.thinkingEffort}
        </Readout>
      )}
    </>
  );
}
