
/**
 * One key-value annotation on a session. Annotations ride as a vec of
 * entries (fluorite has no map type); keys are unique per session.
 */
export interface AnnotationEntry {
  key: string;
  value: string;
}