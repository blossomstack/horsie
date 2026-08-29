
import { ArtifactRef } from '../agent';
/**
 * A message the user is sending, and what they attached to it.
 *
 * `artifacts` are ids the client already uploaded — the bytes went up
 * separately, so sending stays a small JSON request and the composer can
 * show a thumbnail while the turn is still being typed. The server
 * re-resolves every id against this project before accepting, so a client
 * cannot name another project's bytes.
 */
export interface SendMessageRequest {
  text: string;
  /**
   * Defaulted: every client that predates attachments sends `{"text": ...}`
   * alone, and a required field here answers all of them with a 422.
   */
  artifacts: ArtifactRef[];
}