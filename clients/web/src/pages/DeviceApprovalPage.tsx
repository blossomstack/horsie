import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { useSearchParams } from "react-router-dom";
import { ApiRequestError, api } from "../api/client";
import { useTranslation } from "react-i18next";

/** Approve or deny a `horsie auth login` waiting on a device code. */
export function DeviceApprovalPage() {
  const { t } = useTranslation();
  const [params] = useSearchParams();
  const [code, setCode] = useState(params.get("code") ?? "");

  // Both render their failure on the page itself.
  const approve = useMutation({
    mutationFn: () => api.auth.approveDevice(code),
    meta: { inlineError: true },
  });
  const deny = useMutation({
    mutationFn: () => api.auth.denyDevice(code),
    meta: { inlineError: true },
  });

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
            {t("device.title")}
          </h1>
          <p className="mt-0.5 text-xs text-faint">
{t("device.desc")}
          </p>
        </div>

        {approve.isSuccess ? (
          <p data-testid="device-approved" className="text-sm text-lamp-ok">
{t("device.approved")}
          </p>
        ) : deny.isSuccess ? (
          <p data-testid="device-denied" className="text-sm text-legend">
{t("device.denied")}
          </p>
        ) : (
          <>
            <input
              className="field text-center font-mono text-lg tracking-[0.3em]"
              data-testid="device-code"
              value={code}
              onChange={(e) => setCode(e.target.value)}
              placeholder={t("device.codePlaceholder")}
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
                {t("device.approve")}
              </button>
              <button
                className="key flex-1 justify-center"
                data-testid="device-deny"
                disabled={!code || deny.isPending}
                onClick={() => deny.mutate()}
              >
                {t("device.deny")}
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
