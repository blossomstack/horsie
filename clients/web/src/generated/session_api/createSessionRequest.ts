
import { ArtifactRef } from '../agent';
import { EnvironmentSpec } from '../environments';
import { AgentSettings } from '../session';
/**
 * A session is created *with* the first thing to say to it. There is no
 * create-then-message shape: a session with no message is a provisioned
 * runtime nobody asked a question, and nothing reclaims one.
 */
export interface CreateSessionRequest {
  name?: string;
  agent: AgentSettings;
  /**
   * The first user message, queued as part of the create. Required and
   * non-empty.
   */
  message: string;
  /**
   * What came attached to that first message — images and documents already
   * uploaded to this project, named by id. Same shape and same rule as
   * `SendMessageRequest.artifacts`: the bytes went up separately, and the
   * server re-resolves every id against this project before accepting.
   *
   * Defaulted, so every client that creates a session with `{"message": ...}`
   * alone keeps working.
   */
  artifacts: ArtifactRef[];
  /**
   * Where this session runs and what it runs against. Required — a session
   * that did not say has not chosen, it has been chosen for.
   */
  environment: EnvironmentSpec;
  /**
   * Selected plugin-bundle names to provision for this session; absent →
   * the server's default-enabled bundles. Non-empty implies plugins are on.
   */
  plugins?: string[];
}