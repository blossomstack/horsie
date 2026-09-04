
import { EfficiencyStats } from '../session';
import { UsageView } from '../session';
import { EfficiencyDiagnostic } from './efficiencyDiagnostic';
import { SessionEfficiencyAgent } from './sessionEfficiencyAgent';
export interface SessionEfficiencyReport {
  sessionId: string;
  usageTotal: UsageView;
  efficiencyTotal: EfficiencyStats;
  diagnostic: EfficiencyDiagnostic;
  agents: SessionEfficiencyAgent[];
}