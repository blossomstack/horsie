
import { AnnotationEntry } from './annotationEntry';
import { SessionStatusKind } from './sessionStatusKind';
export interface SessionSummary {
  id: string;
  name?: string;
  /**
   * Absent when the session is not loaded: the server does not guess, and
   * a restart leaves every row unknown until one is opened.
   */
  status?: SessionStatusKind;
  createdAt: number;
  lastError?: string;
  /**
   * The workflow this session is a run of. Present only for a run, which is
   * what lets the session list annotate one without a second request.
   * Routine runs carry nothing here — they are not in the list at all.
   */
  workflow?: string;
  /**
   * User-set key-value metadata (e.g. `group=<name>`). Empty when none.
   */
  annotations: AnnotationEntry[];
}