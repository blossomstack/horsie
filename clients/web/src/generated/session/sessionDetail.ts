
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
   * User-set key-value metadata (e.g. `group=<name>`). Empty when none.
   */
  annotations: AnnotationEntry[];
  /**
   * Every question the agent is awaiting an answer to, oldest first. All of
   * them must be answered in one request before the turn can resume.
   */
  pendingAsks: PendingAskView[];
  model: string;
  vendor: string;
  /**
   * Clone URLs of the session's provisioned repos (empty when none).
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
   * Whether the runtime's plugin/skill machinery is enabled for this session.
   */
  usePlugins: boolean;
  /**
   * The session's frozen thinking effort, chosen at creation or inherited
   * from the model's default. Absent → the model exposes no thinking
   * control.
   */
  thinkingEffort?: string;
  /**
   * Messages accepted but not yet carried into a turn, oldest first (empty
   * when nothing is owed).
   */
  inbox: QueuedMessage[];
  /**
   * Token usage summed across every agent this session hosts. Per-agent
   * numbers (and context size, which is never summed) are on the agent
   * document instead.
   */
  usageTotal: UsageView;
  /**
   * Every agent this session hosts: the main agent first, then its subagent
   * tree. Each is addressable at `/sessions/:id/agents/:agent_id`.
   */
  agents: SubAgentView[];
  /**
   * The resource-preparation stage a turn is currently at, when one is
   * spinning up. Live-only history: past stages are not replayable.
   */
  progression?: ProgressionEvent;
  /**
   * The workflow this session is a run of, if it is one. Decides which view
   * the page renders: a run has a graph rather than a conversation.
   */
  workflow?: string;
}