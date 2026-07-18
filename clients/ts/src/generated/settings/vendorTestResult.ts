
/**
 * The outcome of an on-demand connection check for a configurable vendor
 */
export interface VendorTestResult {
  ok: boolean;
  /**
   * The authenticated identity when `ok` (e.g. &quot;admin&quot;, &quot;worker:name&quot;).
   */
  identity?: string;
  error?: string;
}