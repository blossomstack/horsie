
import { QueuedMessage } from './queuedMessage';
import { SessionStatusKind } from './sessionStatusKind';
export interface SessionDetail {
  id: string;
  name?: string;
  status?: SessionStatusKind;
  createdAt: number;
  lastError?: string;
  /**
   * The question the agent is awaiting an answer to (status AwaitingInput).
   */
  pendingQuestion?: string;
  model: string;
  vendor: string;
  /**
   * Clone URLs of the session&#x27;s provisioned repos (empty when none).
   */
  repos: string[];
  /**
   * Selected skill-bundle names (empty when none).
   */
  plugins: string[];
  /**
   * Enabled MCP server names (empty when none).
   */
  mcpServers: string[];
  /**
   * Selected memory space names (empty when none).
   */
  memorySpaces: string[];
  /**
   * Whether the runtime&#x27;s plugin/skill machinery is enabled for this session.
   */
  usePlugins: boolean;
  /**
   * Messages accepted but not yet carried into a turn, oldest first (empty
   */
  inbox: QueuedMessage[];
}