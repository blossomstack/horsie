
import { EnvironmentSpec } from '../environments';
/**
 * Invoke a preset: create a session from it and queue a first message.
 */
export interface AgentInvokeRequest {
  /**
   * First user message; queued immediately after the session is created.
   */
  message: string;
  /**
   * Where this invocation runs and what it runs against. Required.
   */
  environment: EnvironmentSpec;
  /**
   * Optional session title.
   */
  name?: string;
}