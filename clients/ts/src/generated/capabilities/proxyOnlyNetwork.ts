
/**
 * Outbound TCP restricted to `localhost:&#60;port&#62;` only — all other egress is
 */
export interface ProxyOnlyNetwork {
  port: number;
}