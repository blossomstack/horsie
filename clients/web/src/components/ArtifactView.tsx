import { FileText } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "../api/client";
import type { ArtifactRef } from "../api/types";
import { Lightbox } from "./Lightbox";

/** The longest edge a transcript thumbnail is drawn at. */
const MAX_EDGE = 320;

/**
 * What an image should occupy before its bytes have arrived.
 *
 * Read from the reference's own header-derived dimensions, so the box is the
 * right size from the first frame and the transcript does not jump when the
 * picture loads. Those dimensions are optional — the server only fills them in
 * when the header parsed — and `undefined` here means "let the image size
 * itself", which costs one shift rather than drawing the picture wrong.
 */
export function thumbBox(
  ref: ArtifactRef,
): { width: number; height: number } | undefined {
  if (ref.kind.kind !== "Image") return undefined;
  const { width, height } = ref.kind.value;
  if (!width || !height || width <= 0 || height <= 0) return undefined;
  const scale = Math.min(1, MAX_EDGE / Math.max(width, height));
  return {
    width: Math.round(width * scale),
    height: Math.round(height * scale),
  };
}

/** `1.4 MB`. Decimal units, because that is what a file manager shows. */
export function formatBytes(n: number): string {
  if (n < 1000) return `${n} B`;
  const units = ["kB", "MB", "GB"];
  let value = n / 1000;
  let unit = 0;
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000;
    unit++;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

/** What to call a file nobody named — a pasted screenshot has no filename. */
function label(ref: ArtifactRef, fallback: string): string {
  return ref.filename ?? fallback;
}

/**
 * One artifact as the transcript shows it: a thumbnail for a picture, a chip
 * for a document.
 *
 * Two renderings rather than one, because they answer different questions. An
 * image is the content — you read it where it sits — and a PDF is a thing to
 * open, so all it can usefully show is its name and its weight.
 */
export function ArtifactView({ artifact }: { artifact: ArtifactRef }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const href = api.artifactUrl(artifact.id);
  const name = label(artifact, t("artifact.untitled"));

  if (artifact.kind.kind === "Image") {
    const box = thumbBox(artifact);
    return (
      <>
        <button
          type="button"
          onClick={() => setOpen(true)}
          className="block overflow-hidden rounded-[var(--radius-control)] bg-raised transition-shadow focus-visible:shadow-[0_0_0_2px_var(--focus-ring)] focus-visible:outline-none"
          style={box}
          title={t("artifact.openFull")}
          data-testid="artifact-image"
          data-artifact-id={artifact.id}
        >
          <img
            src={href}
            alt={name}
            width={box?.width}
            height={box?.height}
            // Reserved space, filled: the button already holds the box, and
            // the image fills it exactly because the box came from this
            // image's own aspect ratio.
            className="block h-full w-full object-cover"
            style={box ? undefined : { maxWidth: MAX_EDGE }}
          />
        </button>
        {open && (
          <Lightbox
            src={href}
            name={name}
            onClose={() => setOpen(false)}
          />
        )}
      </>
    );
  }

  return (
    <a
      href={href}
      download={name}
      className="inline-flex max-w-full items-center gap-2 rounded-[var(--radius-control)] bg-raised px-2.5 py-2 transition-colors hover:bg-screen focus-visible:shadow-[0_0_0_2px_var(--focus-ring)] focus-visible:outline-none"
      data-testid="artifact-document"
      data-artifact-id={artifact.id}
    >
      <FileText size={14} className="shrink-0 text-faint" aria-hidden />
      <span className="min-w-0 truncate text-[0.8125rem] text-legend">
        {name}
      </span>
      <span className="legend shrink-0">{formatBytes(artifact.byteSize)}</span>
    </a>
  );
}

/** A message's or a tool result's artifacts, in the order it carries them. */
export function ArtifactRow({
  artifacts,
  className,
}: {
  artifacts: ArtifactRef[];
  className?: string;
}) {
  if (artifacts.length === 0) return null;
  return (
    <div
      className={className ?? "flex flex-wrap items-start gap-2"}
      data-testid="artifact-row"
    >
      {artifacts.map((a, i) => (
        // Keyed by id *and* position: an id is a content hash, so the same
        // image attached twice is the same id twice.
        <ArtifactView key={`${a.id}:${i}`} artifact={a} />
      ))}
    </div>
  );
}
