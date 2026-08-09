
import { RuntimeSpec } from './runtimeSpec';
export interface CreateRuntimeRequest {
  runtimeId: string;
  spec: RuntimeSpec;
}