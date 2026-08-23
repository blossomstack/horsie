import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { AgentInfoPanel, type SelectedAgent } from "./AgentInfoPanel";

afterEach(cleanup);

const agent: SelectedAgent = {
  id: "a1",
  title: "audit",
  kind: "subagent",
  status: "completed",
  input: "Check the tree.\n\n- one\n- two",
  output: "### Findings\n\nA **stale** pin in `Cargo.lock`.",
  startedAtMs: 0,
  endedAtMs: 0,
  opens: true,
};

/**
 * A brief is written by an agent and a result by a model, so both arrive as
 * markdown. Held as plain text they rendered as their own source — the one
 * rendering nobody wanted for the two blocks in this panel read at length.
 *
 * The waits are for `Prose`, which loads the markdown chunk lazily: without
 * them these assert against the raw-text Suspense fallback, which is exactly
 * the state being ruled out.
 */
describe("AgentInfoPanel prose", () => {
  const show = () =>
    render(<AgentInfoPanel agent={agent} onClose={() => {}} onOpenTranscript={() => {}} />);

  it("renders the task as markdown, not as its source", async () => {
    show();
    const task = screen.getByTestId("agent-panel-input");
    await waitFor(() => expect(task.querySelectorAll("li")).toHaveLength(2));
    expect(task.textContent).not.toContain("- one");
  });

  it("renders the result as markdown, not as its source", async () => {
    show();
    const result = screen.getByTestId("agent-panel-output");
    await waitFor(() => expect(result.querySelector("h3")?.textContent).toBe("Findings"));
    expect(result.querySelector("strong")?.textContent).toBe("stale");
    expect(result.querySelector("code")?.textContent).toBe("Cargo.lock");
    expect(result.textContent).not.toContain("**stale**");
  });

  it("scales the prose to the panel rather than the transcript", async () => {
    show();
    // Both blocks, so a compact flag threaded to one and dropped on the other
    // cannot pass. `.prose` alone here is transcript-sized: 15px in an 18rem
    // column, larger than every label around it.
    await waitFor(() =>
      expect(document.querySelectorAll(".prose.prose-compact")).toHaveLength(2),
    );
  });
});
