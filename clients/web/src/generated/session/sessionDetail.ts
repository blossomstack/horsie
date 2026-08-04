
import { AnnotationEntry } from './annotationEntry';
import { PendingAskView } from './pendingAskView';
import { ProgressionEvent } from './progressionEvent';
import { QueuedMessage } from './queuedMessage';
import { SessionStatusKind } from './sessionStatusKind';
import { SubAgentView } from './subAgentView';
import { UsageView } from './usageView';
export interface SessionDetail {
  id: string;
  name?: string;
  status?: SessionStatusKind;
  createdAt: number;
  lastError?: string;
  /**
   * User-set key-value metadata (e.g. `group=&lt;name&gt;`). Empty when none.
   */
  annotations: AnnotationEntry[];
  /**
   * Every question the agent is awaiting an answer to, oldest first. All of
   */
  pendingAsks: PendingAskView[];
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
   * The session&#x27;s frozen thinking effort, chosen at creation or inherited
   */
  thinkingEffort?: string;
  /**
   * Messages accepted but not yet carried into a turn, oldest first (empty
   */
  inbox: QueuedMessage[];
  /**
   * Token usage summed across every agent this session hosts. Per-agent
   */
  usageTotal: UsageView;
  /**
   * Every agent this session hosts: the main agent first, then its subagent
   */
  agents: SubAgentView[];
  /**
   * The resource-preparation stage a turn is currently at, when one is
   */
  progression?: ProgressionEvent;
  /**
   * The workflow this session is a run of, if it is one. Decides which view
   */
  workflow?: string;
}