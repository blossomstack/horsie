
/**
 * Pixel dimensions, read from the file header rather than by decoding.
 *
 * Absent when the header would not parse. They are a layout hint — a client
 * reserves space so the transcript does not jump when a thumbnail loads — so a
 * missing value costs a small shift, where a guessed one would render the
 * image wrong.
 */
export interface ImageArtifact {
  width?: number;
  height?: number;
}