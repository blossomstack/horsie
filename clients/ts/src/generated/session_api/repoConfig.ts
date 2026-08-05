
/**
 * One repository to provision into the session workspace.
 */
export interface RepoConfig {
  /**
   * HTTPS clone URL, e.g. &#34;https://github.com/org/repo&#34;.
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