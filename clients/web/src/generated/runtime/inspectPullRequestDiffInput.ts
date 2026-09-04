
/**
 * A bounded changed-file summary, or one file's patch when `path` is present.
 */
export interface InspectPullRequestDiffInput {
  reference: string;
  path?: string;
}