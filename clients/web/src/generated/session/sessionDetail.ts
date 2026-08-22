
import { AnnotationEntry } from './annotationEntry';
import { SessionStatusKind } from './sessionStatusKind';
import { SubAgentView } from './subAgentView';
import { SubSessionView } from './subSessionView';
import { UsageView } from './usageView';
export interface SessionDetail {
  id: string;
  name?: string;
  status: SessionStatusKind;
  createdAt: number;
  lastError?: string;
  /**
   * User-set key-value metadata (e.g. `group=<name>`). Empty when none.
   */
  annotations: AnnotationEntry[];
  /**
   * The predefined environment this session was created from; absent when it
   * was created from an ad-hoc runtime. `vendor` and `repos` are what it
   * resolved to, and stay the answer to what the session actually got.
   */
  environment?: string;
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
   * Token usage summed across every agent this session hosts. Per-agent
   * numbers (and context size, which is never summed) are on the agent
   * document instead.
   */
  usageTotal: UsageView;
  /**
   * Every agent this session hosts, each addressable at
   * `/sessions/:id/agents/:agent_id`. A session lists its main agent
   * first, then its subagent tree. A workflow run has no main agent — it
   * *is* its steps — so it lists one entry per execution in its run log,
   * labelled with the step that ran.
   */
  agents: SubAgentView[];
  /**
   * The sub sessions branched out of this session, so one read tells a
   * client everything the session hosts.
   *
   * Its own field rather than more entries in `agents`, because a sub
   * session is not a delegated task: it owes nobody a result and it never
   * ends, so it has no end stamp for a `SubAgentView` to carry and no honest
   * way to share the shape. The server keeps the two apart for the same
   * reason — a sub session is deliberately not a `SubAgentTree` node.
   */
  subSessions: SubSessionView[];
  /**
   * The workflow this session is a run of, if it is one. Decides which view
   * the page renders: a run has a graph rather than a transcript.
   */
  workflow?: string;
}