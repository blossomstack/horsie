
import { DocumentArtifact } from './documentArtifact';
import { ImageArtifact } from './imageArtifact';
/**
 * What shape an artifact is, and the facts that only that shape has.
 *
 * A field on the reference rather than a split of it: kind and "has this been
 * stored yet" are independent axes, and nesting them multiplies arms for
 * nothing. Adding audio or video later is one new variant here, and because
 * every provider matches this exhaustively, each of them fails to compile
 * until someone says what it does with the new kind.
 */
export type ArtifactKind =
  | { kind: "Image"; value: ImageArtifact }
  | { kind: "Document"; value: DocumentArtifact };