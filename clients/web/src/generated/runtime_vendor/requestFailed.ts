
/**
 * The failure reply to any request. Carries no runtime id — the `request_id`
 * on the envelope already identifies what failed.
 */
export interface RequestFailed {
  message: string;
}