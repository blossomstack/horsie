
/**
 * Advisory: suspend this runtime if you can. A vendor that cannot suspend
 * does nothing and keeps the runtime alive — that is a correct
 * implementation, and far better than destroying a workspace.
 */
export interface HibernateRuntimeRequest {
  runtimeId: string;
}