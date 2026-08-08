
import { AnnotationEntry } from '../session';
/**
 * Merge-update a session's annotations: every `set` entry upserts a key,
 * every `remove` entry drops one. Keys not mentioned are untouched.
 */
export interface SetAnnotationsRequest {
  set: AnnotationEntry[];
  remove: string[];
}