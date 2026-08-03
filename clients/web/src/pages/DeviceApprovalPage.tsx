import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { useSearchParams } from "react-router-dom";
import { ApiRequestError, api } from "../api/client";

/** Approve or deny a `horsie auth login` waiting on a device code. */
export function DeviceApprovalPage() {
  const [params] = useSearchParams();
  const [code, setCode] = useState(params.get("code") ?? "");

  const approve = useMutation({
    mutationFn: () => api.auth.approveDevice(code),
  });
  const deny = useMutation({ mutationFn: () => api.auth.denyDevice(code) });

  const error = [approve.error, deny.error].find(
    (e): e is ApiRequestError => e instanceof ApiRequestError,
  );

  return (
    <div className="flex h-full items-center justify-center p-6">
      <div
        className="panel w-full max-w-md space-y-4 p-6"
        data-testid="device-page"
      >
        <div>
          <h1 className="page-title">
            Authorize a command-line login
          </h1>
          <p className="mt-0.5 text-xs text-faint">
            Check that this code matches the one your terminal printed.
            Approving grants that machine access to this server as you.
          </p>
        </div>

        {approve.isSuccess ? (
          <p data-testid="device-approved" className="text-sm text-lamp-ok">
            Approved. Your terminal should continue in a few seconds — you can
            close this page.
          </p>
        ) : deny.isSuccess ? (
          <p data-testid="device-denied" className="text-sm text-legend">
            Denied. That login attempt was refused.
          </p>
        ) : (
          <>
            <input
              className="field text-center font-mono text-lg tracking-[0.3em]"
              data-testid="device-code"
              value={code}
              onChange={(e) => setCode(e.target.value)}
              placeholder="XXXX-XXXX"
              autoFocus
            />
            {error && (
              <p data-testid="device-error" className="text-xs text-red-ink">
                {error.message}
              </p>
            )}
            <div className="flex gap-2">
              <button
                className="key key-go flex-1 justify-center"
                data-testid="device-approve"
                disabled={!code || approve.isPending}
                onClick={() => approve.mutate()}
              >
                Approve
              </button>
              <button
                className="key flex-1 justify-center"
                data-testid="device-deny"
                disabled={!code || deny.isPending}
                onClick={() => deny.mutate()}
              >
                Deny
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
