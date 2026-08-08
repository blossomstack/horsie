
/**
 * One repository to provision into the session workspace.
 */
export interface RepoConfig {
  /**
   * HTTPS clone URL, e.g. "https://github.com/org/repo".
   */
  url: string;
  /**
   * Branch, tag, or commit to check out; absent → default branch.
   */
  gitRef?: string;
  /**
   * Directory under the workspace; absent → repo basename (deduped).
   */
  dir?: string;
}