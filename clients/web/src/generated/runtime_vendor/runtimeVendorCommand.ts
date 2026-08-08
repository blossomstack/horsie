
import { CreateRuntimeRequest } from './createRuntimeRequest';
import { DeleteRuntimeRequest } from './deleteRuntimeRequest';
import { GetRuntimeRequest } from './getRuntimeRequest';
import { HibernateRuntimeRequest } from './hibernateRuntimeRequest';
import { QueryRuntimesRequest } from './queryRuntimesRequest';
import { RuntimeRelayRequest } from './runtimeRelayRequest';
import { VendorRegistered } from './vendorRegistered';
import { VendorRejected } from './vendorRejected';
export type RuntimeVendorCommand =
  | { type: "CreateRuntime"; value: CreateRuntimeRequest }
  | { type: "GetRuntime"; value: GetRuntimeRequest }
  | { type: "HibernateRuntime"; value: HibernateRuntimeRequest }
  | { type: "DeleteRuntime"; value: DeleteRuntimeRequest }
  | { type: "QueryRuntimes"; value: QueryRuntimesRequest }
  | { type: "Runtime"; value: RuntimeRelayRequest }
  | { type: "VendorRegistered"; value: VendorRegistered }
  | { type: "VendorRejected"; value: VendorRejected };