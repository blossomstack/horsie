
import { AgentSettings } from '../session';
import { RepoConfig } from './repoConfig';
/**
 * A session is created *with* the first thing to say to it. There is no
 */
export interface CreateSessionRequest {
  name?: string;
  agent: AgentSettings;
  /**
   * The first user message, queued as part of the create. Required and
   */
  message: string;
  /**
   * Runtime vendor name; defaults to &#34;local&#34;.
   */
  vendor?: string;
  /**
   * Repositories cloned into a vendor-managed workspace at provision time.
   */
  repos?: RepoConfig[];
  /**
   * Selected plugin-bundle names to provision for this session; absent →
   */
  plugins?: string[];
}