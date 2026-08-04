import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import { qk } from "./useSessions";

export const groupQk = {
  groups: ["groups"] as const,
};

/** Registered group names. The sidebar unions these with the `group`
 * annotations seen in the session list — an annotation-only group still
 * renders. */
export function useGroupList() {
  return useQuery({
    queryKey: groupQk.groups,
    queryFn: () => api.sessionGroups.list(),
    select: (r) => r.groups.map((g) => g.name),
  });
}

function useInvalidatingMutation<TVars>(
  fn: (vars: TVars) => Promise<unknown>,
  extra: (client: ReturnType<typeof useQueryClient>, vars: TVars) => void = () => {},
) {
  const client = useQueryClient();
  return useMutation({
    mutationFn: fn,
    onSuccess: (_r, vars) => {
      client.invalidateQueries({ queryKey: groupQk.groups });
      client.invalidateQueries({ queryKey: qk.sessions });
      extra(client, vars);
    },
  });
}

export function useCreateGroup() {
  return useInvalidatingMutation((name: string) => api.sessionGroups.create(name));
}

export function useRenameGroup() {
  return useInvalidatingMutation(
    ({ oldName, name }: { oldName: string; name: string }) =>
      api.sessionGroups.rename(oldName, name),
  );
}

export function useDeleteGroup() {
  return useInvalidatingMutation((name: string) => api.sessionGroups.remove(name));
}

/** Move a session: `set: [{key:"group",value}]` into a group, `remove:["group"]`
 * back to Ungrouped. */
export function useSetSessionAnnotations() {
  return useInvalidatingMutation(
    ({
      id,
      set,
      remove,
    }: {
      id: string;
      set: { key: string; value: string }[];
      remove: string[];
    }) => api.sessions.setAnnotations(id, { set, remove }),
    (client, { id }) => client.invalidateQueries({ queryKey: qk.session(id) }),
  );
}
