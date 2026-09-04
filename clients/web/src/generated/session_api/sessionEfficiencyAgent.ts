
import { EfficiencyStats } from '../session';
import { UsageView } from '../session';
import { EfficiencyDiagnostic } from './efficiencyDiagnostic';
export interface SessionEfficiencyAgent {
  id: string;
  parent?: string;
  title?: string;
  /**
   * "main" | "subagent" | "step" | "sub_session"
   */
  kind: string;
  status: string;
  model?: string;
  usage: UsageView;
  contextTokens: number;
  contextWindow?: number;
  efficiency: EfficiencyStats;
  diagnostic: EfficiencyDiagnostic;
}