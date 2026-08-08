
import { RoutineSchedule } from './routineSchedule';
/**
 * A routine as shown to clients: its definition, plus what the schedule and
 * the last trigger did.
 */
export interface RoutineView {
  /**
   * Slug; the id of record, used in API paths.
   */
  name: string;
  description: string;
  /**
   * Name of the agent preset every run is configured from.
   */
  agent: string;
  /**
   * The message queued as each run's first user message.
   */
  prompt: string;
  schedule: RoutineSchedule;
  /**
   * False pauses the timer. The run endpoint and the UI button still work.
   */
  enabled: boolean;
  /**
   * When the timer fires next; absent when nothing is scheduled (a manual
   * routine, a paused one, or a spent `Once`).
   */
  nextRunAtMs?: number;
  /**
   * When a trigger was last attempted.
   */
  lastRunAtMs?: number;
  /**
   * The session the last successful trigger created.
   */
  lastSessionId?: string;
  /**
   * Why the last trigger failed to create a session. A run that started and
   * then failed reports through its session, not here.
   */
  lastError?: string;
  /**
   * Unix epoch seconds.
   */
  createdAt: string;
  updatedAt: string;
}