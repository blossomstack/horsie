import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { setCurrentProject } from "../api/client";
import type { ArtifactRef } from "../api/types";
import { ArtifactRow, ArtifactView, formatBytes, thumbBox } from "./ArtifactView";

afterEach(cleanup);

// The URL builder is scoped to a project and throws without one — a routing
// concern, not something these components decide.
beforeAll(() => setCurrentProject("proj"));

const image = (
  kind: Partial<{ width: number; height: number }> = {
    width: 800,
    height: 600,
  },
): ArtifactRef => ({
  id: "abc123",
  mediaType: "image/png",
  byteSize: 2_400_000,
  kind: { kind: "Image", value: kind },
  filename: "shot.png",
});

const pdf = (): ArtifactRef => ({
  id: "def456",
  mediaType: "application/pdf",
  byteSize: 900,
  kind: { kind: "Document", value: {} },
  filename: "spec.pdf",
});

describe("thumbBox", () => {
  // The whole reason the dimensions are on the wire: the box is right before
  // the bytes arrive, so nothing below the image moves when it loads.
  it("scales the longest edge down and keeps the aspect ratio", () => {
    expect(thumbBox(image({ width: 800, height: 600 }))).toEqual({
      width: 320,
      height: 240,
    });
  });

  it("never scales a small image up", () => {
    expect(thumbBox(image({ width: 40, height: 20 }))).toEqual({
      width: 40,
      height: 20,
    });
  });

  // Absent when the header would not parse, and a guessed box would draw the
  // picture wrong — which costs more than one shift.
  it("has no box for an image whose dimensions are unknown", () => {
    expect(thumbBox(image({}))).toBeUndefined();
    expect(thumbBox(image({ width: 0, height: 0 }))).toBeUndefined();
  });

  it("has no box for a document, which is never a thumbnail", () => {
    expect(thumbBox(pdf())).toBeUndefined();
  });
});

describe("formatBytes", () => {
  it("reads as a file manager reads", () => {
    expect(formatBytes(900)).toBe("900 B");
    expect(formatBytes(2_400_000)).toBe("2.4 MB");
  });
});

describe("ArtifactView", () => {
  it("draws an image at its reserved size and points at the bytes", () => {
    render(<ArtifactView artifact={image()} />);
    const img = screen.getByAltText("shot.png") as HTMLImageElement;
    expect(img.getAttribute("src")).toBe("/api/p/proj/artifacts/abc123");
    expect(img.getAttribute("width")).toBe("320");
    expect(img.getAttribute("height")).toBe("240");
  });

  it("draws a document as a downloadable chip, not a thumbnail", () => {
    render(<ArtifactView artifact={pdf()} />);
    const link = screen.getByTestId("artifact-document") as HTMLAnchorElement;
    expect(link.getAttribute("download")).toBe("spec.pdf");
    expect(screen.queryByTestId("artifact-image")).toBeNull();
  });

  it("opens the full-size view, and Escape closes it", () => {
    render(<ArtifactView artifact={image()} />);
    fireEvent.click(screen.getByTestId("artifact-image"));

    const dialog = screen.getByTestId("lightbox");
    expect(dialog.getAttribute("aria-modal")).toBe("true");
    expect(screen.getByTestId("lightbox-download")).toBeTruthy();
    // The keyboard lands inside on open, so Escape and Tab both reach it.
    expect(document.activeElement).toBe(screen.getByTestId("lightbox-close"));

    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByTestId("lightbox")).toBeNull();
  });

  it("closes the full-size view on a click outside it", () => {
    render(<ArtifactView artifact={image()} />);
    fireEvent.click(screen.getByTestId("artifact-image"));
    fireEvent.click(screen.getByTestId("lightbox-backdrop"));
    expect(screen.queryByTestId("lightbox")).toBeNull();
  });

  it("keeps Tab inside the full-size view", () => {
    render(<ArtifactView artifact={image()} />);
    fireEvent.click(screen.getByTestId("artifact-image"));
    const download = screen.getByTestId("lightbox-download");
    const close = screen.getByTestId("lightbox-close");

    // Close is last in document order, so Tab from it wraps to the first.
    close.focus();
    fireEvent.keyDown(screen.getByTestId("lightbox"), { key: "Tab" });
    expect(document.activeElement).toBe(download);

    // …and Shift+Tab from the first wraps back to the last.
    download.focus();
    fireEvent.keyDown(screen.getByTestId("lightbox"), {
      key: "Tab",
      shiftKey: true,
    });
    expect(document.activeElement).toBe(close);
  });
});

describe("ArtifactRow", () => {
  it("draws nothing at all when there is nothing to draw", () => {
    const { container } = render(<ArtifactRow artifacts={[]} />);
    expect(container.firstChild).toBeNull();
  });

  // An id is a content hash, so the same picture attached twice is one id
  // twice — a key of the id alone would collapse them into one.
  it("draws the same artifact twice when it was carried twice", () => {
    render(<ArtifactRow artifacts={[image(), image()]} />);
    expect(screen.getAllByTestId("artifact-image")).toHaveLength(2);
  });
});

describe("Lightbox placement", () => {
  // Every transcript turn carries `.animate-settle`, a *filling* transform
  // animation, which makes the turn the containing block for `position:
  // fixed`. Rendered in place, the backdrop covered one message and stopped.
  it("renders outside the tree it was opened from", () => {
    const { container } = render(<ArtifactView artifact={image()} />);
    fireEvent.click(screen.getByTestId("artifact-image"));
    const backdrop = screen.getByTestId("lightbox-backdrop");
    expect(container.contains(backdrop)).toBe(false);
    expect(backdrop.parentElement).toBe(document.body);
  });
});
