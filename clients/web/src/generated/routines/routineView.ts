
import { RoutineSchedule } from './routineSchedule';
/**
 * A routine as shown to clients: its definition, plus what the schedule and
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
   * The message queued as each run&#x27;s first user message.
   */
  prompt: string;
  schedule: RoutineSchedule;
  /**
   * False pauses the timer. The run endpoint and the UI button still work.
   */
  enabled: boolean;
  /**
   * When the timer fires next; absent when nothing is scheduled (a manual
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
   */
  lastError?: string;
  /**
   * Unix epoch seconds.
   */
  createdAt: string;
  updatedAt: string;
}