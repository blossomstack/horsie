
import { AllowNetwork } from './allowNetwork';
import { BlockNetwork } from './blockNetwork';
import { ProxyOnlyNetwork } from './proxyOnlyNetwork';
/**
 * What network egress the sandbox permits. `Block` is the safe default.
 */
export type NetworkPolicy =
  | { type: "Block"; value: BlockNetwork }
  | { type: "Allow"; value: AllowNetwork }
  | { type: "ProxyOnly"; value: ProxyOnlyNetwork };