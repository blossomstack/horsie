/** Hand-written, because the scanner is plain ESM: it runs as a CLI in CI and
 * as a unit test, and both need it to stay dependency-free. */
export interface Finding {
  /** Repo-relative path of the file the string is in. */
  rel: string;
  line: number;
  /** `jsx-text`, `attr:<name>`, or `missing-key`. */
  kind: string;
  /** The string itself, or the unknown key. */
  text: string;
}

export function catalogueKeys(): Set<string>;
export function audit(): Finding[];
