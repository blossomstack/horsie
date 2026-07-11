
/**
 * Outbound TCP restricted to `localhost:&lt;port&gt;` only — all other egress is
 */
export interface ProxyOnlyNetwork {
  port: number;
}