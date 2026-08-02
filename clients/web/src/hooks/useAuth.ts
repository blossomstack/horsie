import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect } from "react";
import { api } from "../api/client";
import type { AuthStatus } from "../api/types";

export const AUTH_STATUS_KEY = ["auth", "status"] as const;

/**
 * The server's view of this browser. Never cached across a `401`: the
 * `horsie:unauthorized` event the API client emits refetches it, which is what
 * drops an expired session back to the login page.
 */
export function useAuthStatus() {
  const qc = useQueryClient();
  const query = useQuery<AuthStatus>({
    queryKey: AUTH_STATUS_KEY,
    queryFn: () => api.auth.status(),
    staleTime: 30_000,
    retry: false,
  });

  useEffect(() => {
    const onUnauthorized = () => {
      void qc.invalidateQueries({ queryKey: AUTH_STATUS_KEY });
    };
    window.addEventListener("horsie:unauthorized", onUnauthorized);
    return () =>
      window.removeEventListener("horsie:unauthorized", onUnauthorized);
  }, [qc]);

  return query;
}
