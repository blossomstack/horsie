import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import type { ComponentProps } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  SessionStatusKind,
  type ArtifactRef,
  type CatalogEntryView,
} from "../api/types";
import { Composer } from "./Composer";
import { filterEntries, invocationPrefix } from "./EntryMenu";

// The upload is the one thing in here that talks to a server. Mocked at the
// module rather than at `fetch`, so a test says what the *upload* did — the
// composer's contract with the API client is one call in, one `ArtifactRef`
// back — instead of restating the request the client builds.
const upload = vi.fn<(file: File) => Promise<ArtifactRef>>();
vi.mock("../api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api/client")>();
  return {
    ...actual,
    api: {
      ...actual.api,
      artifacts: { upload: (file: File) => upload(file) },
      // The real one throws unless a project is selected, which is a routing
      // concern this component has no part in.
      artifactUrl: (id: string) => `/api/p/test/artifacts/${id}`,
    },
  };
});

const imageRef = (id = "sha-1"): ArtifactRef => ({
  id,
  mediaType: "image/png",
  byteSize: 1234,
  kind: { kind: "Image", value: { width: 800, height: 600 } },
  filename: "shot.png",
});

const png = (name = "shot.png") =>
  new File(["bytes"], name, { type: "image/png" });

beforeEach(() => {
  upload.mockReset();
  upload.mockResolvedValue(imageRef());
  // jsdom ships no blob-URL implementation; the tray asks for one per image.
  URL.createObjectURL = vi.fn(() => "blob:preview");
  URL.revokeObjectURL = vi.fn();
});

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

const entries: CatalogEntryView[] = [
  {
    kind: "command",
    name: "commit",
    description: "Create a git commit",
    argumentHint: "<msg>",
  },
  { kind: "command", name: "review", description: "Review a pull request" },
  { kind: "agent", name: "reviewer", description: "Reviews a diff" },
];

/** What the composer promises its caller, so a test's stub is checked
 * against the real signature rather than against `any`. */
type SendHandler = (
  text: string,
  artifacts: ArtifactRef[],
) => void | Promise<unknown>;

function composer(
  onSend: SendHandler = vi.fn(),
  props: Partial<ComponentProps<typeof Composer>> = {},
) {
  render(
    <Composer
      status={SessionStatusKind.Idle}
      busy={false}
      entries={entries}
      onSend={onSend}
      onStop={vi.fn()}
      {...props}
    />,
  );
  return {
    input: screen.getByTestId("composer-input"),
    onSend,
  };
}

/** Paste, with files on the clipboard and nothing else — which is exactly
 * what a screenshot on the clipboard looks like. */
const pasteFiles = (input: HTMLElement, files: File[]) =>
  fireEvent.paste(input, { clipboardData: { files, items: [], types: [] } });

describe("invocationPrefix", () => {
  it("matches a leading sigil and only while the name is being typed", () => {
    expect(invocationPrefix("/")).toEqual({ sigil: "/", query: "" });
    expect(invocationPrefix("/rev")).toEqual({ sigil: "/", query: "rev" });
    expect(invocationPrefix("@rev")).toEqual({ sigil: "@", query: "rev" });
    // Arguments have started: the user has moved on.
    expect(invocationPrefix("/review src")).toBeNull();
    // Not leading — the same rule the server's parser uses.
    expect(invocationPrefix("see /review")).toBeNull();
    expect(invocationPrefix("mail a@b.com")).toBeNull();
    expect(invocationPrefix("hello")).toBeNull();
  });
});

describe("filterEntries", () => {
  it("splits by sigil so `@` is not a second `/`", () => {
    expect(filterEntries(entries, "@", "").map((e) => e.name)).toEqual([
      "reviewer",
    ]);
    expect(filterEntries(entries, "/", "").map((e) => e.name)).toEqual([
      "commit",
      "review",
    ]);
  });

  it("matches on name or description", () => {
    expect(filterEntries(entries, "/", "rev").map((e) => e.name)).toEqual([
      "review",
    ]);
    expect(filterEntries(entries, "/", "git").map((e) => e.name)).toEqual([
      "commit",
    ]);
  });
});

describe("Composer typeahead", () => {
  it("opens on a leading slash and not mid-message", () => {
    const { input } = composer();
    fireEvent.change(input, { target: { value: "look at /etc" } });
    expect(screen.queryByTestId("entry-menu")).toBeNull();

    fireEvent.change(input, { target: { value: "/" } });
    expect(screen.getByTestId("entry-menu")).toBeTruthy();
    expect(screen.getByText("/commit")).toBeTruthy();
    // An agent is not reachable from `/`.
    expect(screen.queryByText("@reviewer")).toBeNull();
  });

  /// Enter with the menu open must pick, not send: sending `/rev` because the
  /// menu was up is the mistake the key ordering exists to prevent.
  it("Enter picks while the menu is open and sends once it is closed", () => {
    const { input, onSend } = composer();
    fireEvent.change(input, { target: { value: "/rev" } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(onSend).not.toHaveBeenCalled();
    expect((input as HTMLTextAreaElement).value).toBe("/review ");
    expect(screen.queryByTestId("entry-menu")).toBeNull();

    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("/review", []);
  });

  it("moves the selection with the arrow keys", () => {
    const { input, onSend } = composer();
    fireEvent.change(input, { target: { value: "/" } });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Tab" });
    expect((input as HTMLTextAreaElement).value).toBe("/review ");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("dismisses with Escape without sending", () => {
    const { input, onSend } = composer();
    fireEvent.change(input, { target: { value: "/rev" } });
    fireEvent.keyDown(input, { key: "Escape" });
    expect(screen.queryByTestId("entry-menu")).toBeNull();
    expect(onSend).not.toHaveBeenCalled();
  });

  // Sending while offline used to clear the box and lose the message: the
  // request failed, the optimistic bubble vanished on the next refetch, and
  // what had been typed existed nowhere.
  it("puts the message back when the send never left", async () => {
    const onSend = vi.fn(() => Promise.reject(new Error("offline")));
    const { input } = composer(onSend);
    fireEvent.change(input, { target: { value: "hello" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("hello", []);
    await waitFor(() =>
      expect((input as HTMLTextAreaElement).value).toBe("hello"),
    );
  });

  // But not over something typed while the request was in flight.
  it("does not clobber a new message typed while the send was in flight", async () => {
    let reject: (e: Error) => void = () => {};
    const onSend = vi.fn(
      () => new Promise((_, r) => { reject = r as (e: Error) => void; }),
    );
    const { input } = composer(onSend);
    fireEvent.change(input, { target: { value: "first" } });
    fireEvent.keyDown(input, { key: "Enter" });
    fireEvent.change(input, { target: { value: "second" } });
    reject(new Error("offline"));
    await waitFor(() => expect(onSend).toHaveBeenCalled());
    expect((input as HTMLTextAreaElement).value).toBe("second");
  });

  it("stays out of the way when nothing matches", () => {
    const { input, onSend } = composer();
    fireEvent.change(input, { target: { value: "/nosuch" } });
    expect(screen.queryByTestId("entry-menu")).toBeNull();
    // And Enter sends it verbatim — an unknown name is not an error.
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("/nosuch", []);
  });
});

describe("Composer attachments", () => {
  it("attaches a pasted image and uploads it straight away", async () => {
    const { input } = composer();
    pasteFiles(input, [png()]);

    // Visible before the upload finishes: that is the whole point of
    // uploading on attach rather than on send.
    const item = screen.getByTestId("composer-attachment");
    expect(item.dataset.status).toBe("uploading");
    expect(upload).toHaveBeenCalledTimes(1);

    await waitFor(() =>
      expect(
        screen.getByTestId("composer-attachment").dataset.status,
      ).toBe("ready"),
    );
  });

  it("refuses a file type the server does not store", () => {
    const { input } = composer();
    pasteFiles(input, [
      new File(["x"], "notes.txt", { type: "text/plain" }),
    ]);

    expect(screen.queryByTestId("composer-attachment")).toBeNull();
    expect(upload).not.toHaveBeenCalled();
    expect(screen.getByTestId("composer-attach-notice")).toBeTruthy();
  });

  it("removes an attachment", async () => {
    const { input } = composer();
    pasteFiles(input, [png()]);
    await waitFor(() =>
      expect(
        screen.getByTestId("composer-attachment").dataset.status,
      ).toBe("ready"),
    );

    fireEvent.click(screen.getByTestId("composer-attachment-remove"));
    expect(screen.queryByTestId("composer-attachment")).toBeNull();
  });

  it("hands the send the refs it uploaded", async () => {
    const onSend = vi.fn();
    const { input } = composer(onSend);
    pasteFiles(input, [png()]);
    await waitFor(() =>
      expect(
        screen.getByTestId("composer-attachment").dataset.status,
      ).toBe("ready"),
    );

    fireEvent.change(input, { target: { value: "what is this" } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(onSend).toHaveBeenCalledWith("what is this", [imageRef()]);
    // Sent means gone: the tray is the message, and it left with it.
    expect(screen.queryByTestId("composer-attachment")).toBeNull();
  });

  // An image on its own is a message. Requiring text alongside it would make
  // "look at this" mandatory, which nobody types.
  it("sends an attachment with no text at all", async () => {
    const onSend = vi.fn();
    const { input } = composer(onSend);
    pasteFiles(input, [png()]);
    await waitFor(() =>
      expect(
        screen.getByTestId("composer-attachment").dataset.status,
      ).toBe("ready"),
    );

    fireEvent.click(screen.getByTestId("composer-send"));
    expect(onSend).toHaveBeenCalledWith("", [imageRef()]);
  });

  // The same rule the text follows: a message that never left is still yours.
  it("puts the attachments back when the send is rejected", async () => {
    const onSend = vi.fn(() => Promise.reject(new Error("offline")));
    const { input } = composer(onSend);
    pasteFiles(input, [png()]);
    await waitFor(() =>
      expect(
        screen.getByTestId("composer-attachment").dataset.status,
      ).toBe("ready"),
    );

    fireEvent.change(input, { target: { value: "hello" } });
    fireEvent.keyDown(input, { key: "Enter" });

    await waitFor(() =>
      expect(screen.getByTestId("composer-attachment")).toBeTruthy(),
    );
    expect((input as HTMLTextAreaElement).value).toBe("hello");
  });

  it("will not send while an upload is still in flight", () => {
    upload.mockReturnValue(new Promise(() => {}));
    const onSend = vi.fn();
    const { input } = composer(onSend);
    fireEvent.change(input, { target: { value: "hello" } });
    pasteFiles(input, [png()]);

    expect(
      (screen.getByTestId("composer-send") as HTMLButtonElement).disabled,
    ).toBe(true);
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSend).not.toHaveBeenCalled();
  });

  it("shows the upload's own failure on the attachment", async () => {
    upload.mockRejectedValue(new Error("nope"));
    const { input } = composer();
    pasteFiles(input, [png()]);

    await waitFor(() =>
      expect(screen.getByTestId("composer-attachment-error")).toBeTruthy(),
    );
    expect(screen.getByTestId("composer-attachment").dataset.status).toBe(
      "error",
    );
  });

  it("takes a file chosen through the paperclip", async () => {
    const { input: _input } = composer();
    fireEvent.change(screen.getByTestId("composer-file-input"), {
      target: { files: [png()] },
    });
    await waitFor(() =>
      expect(
        screen.getByTestId("composer-attachment").dataset.status,
      ).toBe("ready"),
    );
  });

  it("takes a drop onto the composer", async () => {
    composer();
    fireEvent.drop(screen.getByTestId("composer-panel"), {
      dataTransfer: { files: [png()], types: ["Files"] },
    });
    await waitFor(() =>
      expect(
        screen.getByTestId("composer-attachment").dataset.status,
      ).toBe("ready"),
    );
  });

  // The capability flags are the model's answer, not the composer's: when it
  // cannot read a picture there is nothing to attach, and the control has to
  // say so rather than accept a file that will be dropped.
  it("disables attaching with a reason when nothing may be attached", () => {
    const { input } = composer(vi.fn(), {
      canAttachImages: false,
      canAttachDocuments: false,
    });
    const button = screen.getByTestId("composer-attach") as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    expect(button.title).toBeTruthy();

    pasteFiles(input, [png()]);
    expect(upload).not.toHaveBeenCalled();
    expect(screen.getByTestId("composer-attach-notice").textContent).toBe(
      button.title,
    );
  });

  it("refuses a PDF where only images are allowed", () => {
    const { input } = composer(vi.fn(), { canAttachDocuments: false });
    pasteFiles(input, [
      new File(["%PDF"], "spec.pdf", { type: "application/pdf" }),
    ]);
    expect(upload).not.toHaveBeenCalled();
    expect(screen.getByTestId("composer-attach-notice")).toBeTruthy();
  });
});
