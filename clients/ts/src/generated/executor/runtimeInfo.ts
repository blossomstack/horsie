
import { RuntimeState } from './runtimeState';
/**
 * Runtime summary returned by QueryRuntimes
 */
export interface RuntimeInfo {
  runtimeId: string;
  state: RuntimeState;
  restartCount: number;
}