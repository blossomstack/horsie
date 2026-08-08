
import { RepoConfig } from '../session_api';
/**
 * The ad-hoc environment: a runtime vendor, plus the repos to check out into
 * it when it can provision a workspace.
 *
 * There is no separate variant for the local runtime. `vendor: "local"`
 * already says it, and a vendor that cannot provision rejects a non-empty
 * `repos` at create — a `Local` variant would be a second way to say the same
 * thing, with a vendor name baked into the protocol.
 */
export interface RuntimeEnvironment {
  vendor: string;
  repos?: RepoConfig[];
}