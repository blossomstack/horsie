import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  within,
} from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useState } from "react";
import type {
  RenderedMessage,
  RenderedToolCall,
  TranscriptItem,
} from "../hooks/useSessionStream";
import { Transcript } from "./Transcript";
import {
  formatTranscriptComments,
  transcriptSelection,
  type TranscriptComment,
} from "./TranscriptComments";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

const assistant: RenderedMessage = {
  id: "a1",
  role: "Assistant",
  text: "Keep the retry limit at three attempts.",
  thinking: [],
  toolCalls: [],
  subagentResults: [],
  artifacts: [],
  createdAtMs: 1_000,
};

function tool(id: string): RenderedToolCall {
  return {
    id,
    name: "read_file",
    input: {},
    running: false,
    hooks: [],
    artifacts: [],
  };
}

function CommentableTranscript({
  items = [{ kind: "message", value: assistant }],
  streaming = "",
  showLive = false,
}: {
  items?: TranscriptItem[];
  streaming?: string;
  showLive?: boolean;
}) {
  const [comments, setComments] = useState<TranscriptComment[]>([]);
  const [pending, setPending] = useState(false);
  return (
    <>
      <Transcript
        items={items}
        streaming={streaming}
        orphanTools={[]}
        showLive={showLive}
        showThinking={false}
        sessionId="s1"
        commenting={{
          comments,
          onAdd: (comment) => setComments((current) => [...current, comment]),
          onUpdate: (id, comment) =>
            setComments((current) =>
              current.map((item) =>
                item.id === id ? { ...item, comment } : item,
              ),
            ),
          onRemove: (id) =>
            setComments((current) => current.filter((item) => item.id !== id)),
          onPendingChange: setPending,
        }}
      />
      <output data-testid="comment-state" hidden>
        {comments.map((item) => item.comment).join("|")}
      </output>
      <output data-testid="comment-pending" hidden>
        {String(pending)}
      </output>
    </>
  );
}

function makeSelection(text: string, start: number, end: number): HTMLElement {
  const element = screen.getByText(text);
  const node = element.firstChild;
  if (!node) throw new Error("test text has no text node");
  const range = document.createRange();
  range.setStart(node, start);
  range.setEnd(node, end);
  const selection = window.getSelection();
  selection?.removeAllRanges();
  selection?.addRange(range);
  return element;
}

function selectText(text: string, start: number, end: number) {
  fireEvent.mouseUp(makeSelection(text, start, end));
}

describe("transcript comments", () => {
  it("keeps a work group open while new work is appended", () => {
    const first: RenderedMessage = {
      ...assistant,
      id: "a1",
      text: "",
      thinking: ["finding the config"],
      toolCalls: [tool("t1")],
    };
    const next: RenderedMessage = {
      ...assistant,
      id: "a2",
      text: "",
      thinking: ["checking the result"],
      toolCalls: [tool("t2")],
    };
    const props = {
      streaming: "",
      orphanTools: [],
      showLive: false,
      showThinking: true,
      sessionId: "s1",
    };
    const { rerender } = render(
      <Transcript items={[{ kind: "message", value: first }]} {...props} />,
    );

    fireEvent.click(screen.getByTestId("work-group-toggle"));
    expect(screen.getByTestId("work-group-toggle").getAttribute("aria-expanded")).toBe(
      "true",
    );

    rerender(
      <Transcript
        items={[
          { kind: "message", value: first },
          { kind: "message", value: next },
        ]}
        {...props}
      />,
    );

    expect(screen.getByTestId("work-group-toggle").getAttribute("aria-expanded")).toBe(
      "true",
    );
  });

  it("opens the comment field when a keyboard or touch selection settles", async () => {
    vi.useFakeTimers();
    render(<CommentableTranscript />);
    makeSelection(assistant.text, 5, 20);
    fireEvent(document, new Event("selectionchange"));
    await act(async () => vi.advanceTimersByTimeAsync(250));

    expect(screen.getByTestId("transcript-comment-panel")).toBeTruthy();
    expect(screen.getByTestId("transcript-comment-marker")).toBeTruthy();
  });

  it("does not comment on text that is still streaming", () => {
    render(<CommentableTranscript showLive streaming="unfinished response" />);
    selectText("unfinished response", 0, 10);

    expect(screen.queryByTestId("transcript-comment-panel")).toBeNull();
    expect(screen.queryByTestId("transcript-comment-marker")).toBeNull();
  });

  it("opens a floating field for selected text and leaves a marker when saved", () => {
    render(<CommentableTranscript />);

    selectText(assistant.text, 5, 20);
    expect(screen.getByTestId("transcript-comment-panel").textContent).toContain(
      "the retry limit",
    );

    fireEvent.change(screen.getByLabelText("Add a comment…"), {
      target: { value: "Make this five." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));

    expect(screen.queryByTestId("transcript-comment-panel")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Open comment on “the retry limit”" }));
    const panel = screen.getByTestId("transcript-comment-panel");
    expect(panel.textContent).toContain("Make this five.");
    expect(panel.textContent).toContain("the retry limit");
  });

  it("edits and removes a saved comment from its marker", async () => {
    render(<CommentableTranscript />);
    selectText(assistant.text, 5, 20);
    fireEvent.change(screen.getByLabelText("Add a comment…"), {
      target: { value: "Make this five." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));

    fireEvent.click(screen.getByRole("button", { name: "Open comment on “the retry limit”" }));
    const editor = screen.getByLabelText("Edit comment");
    fireEvent.change(editor, {
      target: { value: "Keep this at four.\nDocument the limit." },
    });
    expect(screen.getByTestId("comment-state").textContent).toBe("Make this five.");
    expect(screen.getByTestId("comment-pending").textContent).toBe("true");
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.getByTestId("comment-state").textContent).toBe(
      "Keep this at four.\nDocument the limit.",
    );
    expect(screen.getByTestId("comment-pending").textContent).toBe("false");

    fireEvent.click(screen.getByRole("button", { name: "Open comment on “the retry limit”" }));
    const panel = screen.getByTestId("transcript-comment-panel");
    expect(panel.textContent).toContain("Keep this at four.\nDocument the limit.");
    expect(panel.querySelector("p")?.className).toContain("whitespace-pre-wrap");
    fireEvent.click(screen.getByRole("button", { name: "Remove" }));
    expect(screen.queryByTestId("transcript-comment-marker")).toBeNull();
    expect(screen.queryByTestId("transcript-comment-panel")).toBeNull();
    await act(async () => new Promise((resolve) => setTimeout(resolve, 0)));
    expect((document.activeElement as HTMLElement).dataset.commentAnchor).toBe("a1");
  });
  it("collapses from Escape when a panel action has focus", () => {
    render(<CommentableTranscript />);
    selectText(assistant.text, 5, 20);
    fireEvent.change(screen.getByLabelText("Add a comment…"), {
      target: { value: "Make this five." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));
    fireEvent.click(
      screen.getByRole("button", { name: "Open comment on “the retry limit”" }),
    );

    fireEvent.keyDown(screen.getByRole("button", { name: "Remove" }), {
      key: "Escape",
    });
    expect(screen.queryByTestId("transcript-comment-panel")).toBeNull();
    expect(screen.getByTestId("transcript-comment-marker")).toBeTruthy();
  });

  it("keeps an in-progress edit when older assistant history is prepended", () => {
    const { rerender } = render(<CommentableTranscript />);
    selectText(assistant.text, 5, 20);
    fireEvent.change(screen.getByLabelText("Add a comment…"), {
      target: { value: "Make this five." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));
    fireEvent.click(screen.getByRole("button", { name: "Open comment on “the retry limit”" }));
    fireEvent.change(screen.getByLabelText("Edit comment"), {
      target: { value: "Unsaved wording" },
    });

    const older: RenderedMessage = {
      ...assistant,
      id: "a0",
      text: "Earlier assistant context.",
      createdAtMs: 500,
    };
    rerender(
      <CommentableTranscript
        items={[
          { kind: "message", value: older },
          { kind: "message", value: assistant },
        ]}
      />,
    );

    expect((screen.getByLabelText("Edit comment") as HTMLTextAreaElement).value).toBe(
      "Unsaved wording",
    );
  });

  it("keeps an in-progress edit when the assistant turn becomes live", () => {
    const { rerender } = render(<CommentableTranscript />);
    selectText(assistant.text, 5, 20);
    fireEvent.change(screen.getByLabelText("Add a comment…"), {
      target: { value: "Make this five." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));
    fireEvent.click(screen.getByRole("button", { name: "Open comment on “the retry limit”" }));
    fireEvent.change(screen.getByLabelText("Edit comment"), {
      target: { value: "Edit while the next call starts" },
    });

    rerender(
      <CommentableTranscript showLive streaming="A new response is arriving" />,
    );

    expect((screen.getByLabelText("Edit comment") as HTMLTextAreaElement).value).toBe(
      "Edit while the next call starts",
    );

    const next: RenderedMessage = {
      ...assistant,
      id: "a2",
      text: "The new response settled.",
      createdAtMs: 1_500,
    };
    rerender(
      <CommentableTranscript
        items={[
          { kind: "message", value: assistant },
          { kind: "message", value: next },
        ]}
      />,
    );
    expect((screen.getByLabelText("Edit comment") as HTMLTextAreaElement).value).toBe(
      "Edit while the next call starts",
    );
  });

  it("keeps multiple comments collapsed behind markers and opens only one", () => {
    render(<CommentableTranscript />);
    selectText(assistant.text, 5, 20);
    fireEvent.change(screen.getByLabelText("Add a comment…"), {
      target: { value: "First comment" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));

    const secondStart = assistant.text.indexOf("three attempts");
    selectText(
      assistant.text,
      secondStart,
      secondStart + "three attempts".length,
    );
    fireEvent.change(screen.getByLabelText("Add a comment…"), {
      target: { value: "Second comment" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));

    expect(screen.getAllByTestId("transcript-comment-marker")).toHaveLength(2);
    fireEvent.click(screen.getByRole("button", { name: "Open comment on “the retry limit”" }));
    expect(screen.getAllByTestId("transcript-comment-panel")).toHaveLength(1);
    expect(screen.getByTestId("transcript-comment-panel").textContent).toContain(
      "First comment",
    );
    fireEvent.change(screen.getByLabelText("Edit comment"), {
      target: { value: "First comment, revised" },
    });

    fireEvent.click(screen.getByRole("button", { name: "Open comment on “three attempts”" }));
    expect(screen.getAllByTestId("transcript-comment-panel")).toHaveLength(1);
    expect(screen.getByTestId("transcript-comment-panel").textContent).toContain(
      "Second comment",
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Open comment on “the retry limit”" }),
    );
    expect((screen.getByLabelText("Edit comment") as HTMLTextAreaElement).value).toBe(
      "First comment, revised",
    );
    fireEvent.click(
      within(screen.getByTestId("transcript-comment-panel")).getByRole("button", {
        name: "Collapse comment",
      }),
    );
    expect(screen.queryByTestId("transcript-comment-panel")).toBeNull();
    expect(screen.getByTestId("comment-state").textContent).toBe(
      "First comment|Second comment",
    );
    expect(screen.getByTestId("comment-pending").textContent).toBe("true");
    expect(
      screen.getByRole("button", { name: "Open comment on “the retry limit”" })
        .dataset.pending,
    ).toBe("true");
    fireEvent.click(
      screen.getByRole("button", { name: "Open comment on “the retry limit”" }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.getByTestId("comment-pending").textContent).toBe("false");
  });

  it("rejects a selection that crosses transcript turns", () => {
    const root = document.createElement("div");
    root.innerHTML =
      '<div data-comment-anchor="one">first</div><div data-comment-anchor="two">second</div>';
    document.body.append(root);
    const first = root.firstElementChild?.firstChild;
    const second = root.lastElementChild?.firstChild;
    if (!first || !second) throw new Error("test turns are missing");
    const range = document.createRange();
    range.setStart(first, 0);
    range.setEnd(second, 6);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);

    expect(transcriptSelection(root, selection)).toBeNull();
    root.remove();
  });
});

describe("formatTranscriptComments", () => {
  it("pairs every blockquoted excerpt with its comment", () => {
    const text = formatTranscriptComments([
      {
        id: "c1",
        anchorId: "a1",
        quote: "first line\nsecond line",
        comment: "Please revise this.",
      },
      {
        id: "c2",
        anchorId: "a2",
        quote: "another excerpt",
        comment: "Keep this part.",
      },
    ]);

    expect(text).toBe(
      "> first line\n> second line\nPlease revise this.\n\n> another excerpt\nKeep this part.",
    );
  });
});
