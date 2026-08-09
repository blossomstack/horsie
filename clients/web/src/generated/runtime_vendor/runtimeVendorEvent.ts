
import { CreateRuntimeResponse } from './createRuntimeResponse';
import { DeleteRuntimeResponse } from './deleteRuntimeResponse';
import { GetRuntimeResponse } from './getRuntimeResponse';
import { HibernateRuntimeResponse } from './hibernateRuntimeResponse';
import { QueryRuntimesResponse } from './queryRuntimesResponse';
import { RequestFailed } from './requestFailed';
import { RuntimeRelayResponse } from './runtimeRelayResponse';
import { RuntimeStateChanged } from './runtimeStateChanged';
import { RuntimeVendorReady } from './runtimeVendorReady';
export type RuntimeVendorEvent =
  | { type: "Ready"; value: RuntimeVendorReady }
  | { type: "CreateRuntime"; value: CreateRuntimeResponse }
  | { type: "GetRuntime"; value: GetRuntimeResponse }
  | { type: "HibernateRuntime"; value: HibernateRuntimeResponse }
  | { type: "DeleteRuntime"; value: DeleteRuntimeResponse }
  | { type: "QueryRuntimes"; value: QueryRuntimesResponse }
  | { type: "Runtime"; value: RuntimeRelayResponse }
  | { type: "RequestFailed"; value: RequestFailed }
  | { type: "RuntimeStateChanged"; value: RuntimeStateChanged };