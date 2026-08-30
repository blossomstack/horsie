
import { ArtifactKind } from './artifactKind';
/**
 * An image or document a message carries, named rather than embedded.
 *
 * This is the *only* artifact shape a message ever holds — in the journal, in
 * agent state, over SSE and over the HTTP API alike. The bytes live in the
 * artifact service and are fetched by `id`.
 *
 * Why never the bytes: `AgentState.log` is the whole transcript and is
 * snapshotted as one blob, so an inlined image would be rewritten into every
 * later snapshot of that agent and re-sent on every backfill. Because this
 * type cannot carry bytes, that is not a rule anyone has to remember.
 */
export interface ArtifactRef {
  /**
   * Lowercase-hex sha256 of the bytes. Content-addressed, so the same image
   * sent twice is stored once and a fetch URL is safe to cache for ever.
   */
  id: string;
  /**
   * The type the server sniffed from the bytes. Never the caller's claim:
   * a browser's `Content-Type` and an MCP block's `mimeType` are both only
   * assertions about content the server has in hand.
   */
  mediaType: string;
  kind: ArtifactKind;
  byteSize: number;
  /**
   * What the client called it. Absent for a paste, which has no filename.
   */
  filename?: string;
}