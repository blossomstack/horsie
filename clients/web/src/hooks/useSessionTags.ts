import { useMutation, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import { TAG_PREFIX } from "../lib/sessionTags";
import { qk } from "./useSessions";

/** Assign or unassign one tag. Both directions are the same annotation
 * merge-update, which is why tags need no endpoint of their own — and why
 * assigning a name nobody has used before is all it takes to create one. */
export function useSetSessionTag() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ id, tag, on }: { id: string; tag: string; on: boolean }) =>
      api.sessions.setAnnotations(id, {
        set: on ? [{ key: `${TAG_PREFIX}${tag}`, value: "" }] : [],
        remove: on ? [] : [`${TAG_PREFIX}${tag}`],
      }),
    onSuccess: (_r, { id }) => {
      client.invalidateQueries({ queryKey: qk.sessions });
      client.invalidateQueries({ queryKey: qk.session(id) });
    },
  });
}
