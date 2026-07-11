
import { Usage } from '../agent';
export interface TurnCompletedEvent {
  iterations: number;
  usage: Usage;
}