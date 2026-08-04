
import { AnnotationEntry } from '../session';
/**
 * Merge-update a session&#x27;s annotations: every `set` entry upserts a key,
 */
export interface SetAnnotationsRequest {
  set: AnnotationEntry[];
  remove: string[];
}