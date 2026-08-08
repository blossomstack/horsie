
/**
 * One environment variable explicitly injected into a spawned runtime child
 * (e.g. a capability token or a synthetic `HOME`).
 */
export interface EnvVar {
  name: string;
  value: string;
}