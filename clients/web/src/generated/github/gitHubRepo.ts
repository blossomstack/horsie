
/**
 * One repository visible to the App installation.
 */
export interface GitHubRepo {
  /**
   * &#34;owner/name&#34;.
   */
  fullName: string;
  private: boolean;
  defaultBranch: string;
}