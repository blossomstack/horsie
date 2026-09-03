import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { useState } from "react";
import type {
  RenderedMessage,
  TranscriptItem,
} from "../hooks/useSessionStream";
import { Transcript } from "./Transcript";
import {
  formatTranscriptComments,
  transcriptSelection,
  type TranscriptComment,
} from "./TranscriptComments";

afterEach(cleanup);

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
        }}
      />
      <output data-testid="comment-state" hidden>
        {comments.map((item) => item.comment).join("|")}
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
  it("opens the comment field when a keyboard or touch selection settles", async () => {
    render(<CommentableTranscript />);
    makeSelection(assistant.text, 5, 20);
    fireEvent(document, new Event("selectionchange"));

    expect(await screen.findByTestId("transcript-comment-draft")).toBeTruthy();
  });

  it("does not comment on text that is still streaming", () => {
    render(<CommentableTranscript showLive streaming="unfinished response" />);
    selectText("unfinished response", 0, 10);

    expect(screen.queryByTestId("transcript-comment-draft")).toBeNull();
  });

  it("attaches a comment field to selected text and keeps the saved comment", () => {
    render(<CommentableTranscript />);

    selectText(assistant.text, 5, 20);
    expect(screen.getByTestId("transcript-comment-draft").textContent).toContain(
      "the retry limit",
    );

    fireEvent.change(screen.getByLabelText("Add a comment…"), {
      target: { value: "Make this five." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));

    const saved = screen.getByTestId("transcript-comment");
    expect(saved.textContent).toContain("Make this five.");
    expect(saved.textContent).toContain("the retry limit");
  });

  it("edits and removes a saved comment", () => {
    render(<CommentableTranscript />);
    selectText(assistant.text, 5, 20);
    fireEvent.change(screen.getByLabelText("Add a comment…"), {
      target: { value: "Make this five." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));

    fireEvent.click(screen.getByRole("button", { name: "Edit comment" }));
    const editor = screen.getByLabelText("Edit comment");
    fireEvent.change(editor, {
      target: { value: "Keep this at four.\nDocument the limit." },
    });
    expect(screen.getByTestId("comment-state").textContent).toBe(
      "Keep this at four.\nDocument the limit.",
    );
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    const saved = screen.getByTestId("transcript-comment");
    expect(saved.textContent).toContain("Keep this at four.\nDocument the limit.");
    expect(saved.querySelector("p")?.className).toContain("whitespace-pre-wrap");

    fireEvent.click(screen.getByRole("button", { name: "Remove comment" }));
    expect(screen.queryByTestId("transcript-comment")).toBeNull();
  });

  it("keeps an in-progress edit when older assistant history is prepended", () => {
    const { rerender } = render(<CommentableTranscript />);
    selectText(assistant.text, 5, 20);
    fireEvent.change(screen.getByLabelText("Add a comment…"), {
      target: { value: "Make this five." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));
    fireEvent.click(screen.getByRole("button", { name: "Edit comment" }));
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
    fireEvent.click(screen.getByRole("button", { name: "Edit comment" }));
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
  it("builds one readable input message from every excerpt and comment", () => {
    const text = formatTranscriptComments(
      [
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
      ],
      { intro: "Review these.", excerpt: "Excerpt", comment: "Comment" },
    );

    expect(text).toBe(
      "Review these.\n\nExcerpt 1:\n> first line\n> second line\n\nComment:\nPlease revise this.\n\nExcerpt 2:\n> another excerpt\n\nComment:\nKeep this part.",
    );
  });
});
