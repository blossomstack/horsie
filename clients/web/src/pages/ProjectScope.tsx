import { setCurrentProject } from "../api/client";
import { useProjects } from "../hooks/useProjects";

/**
 * The project this document was loaded into, from its own URL.
 *
 * `null` on the account-level pages — `/` and `/auth/device` — which is what
 * `App` switches on. Read from `window.location` rather than from a route
 * parameter because the answer is needed *before* a router exists: it is the
 * router's basename.
 *
 * Sets the API client's project as a side effect, which is unusual and
 * deliberate. The client is a plain module with no access to React state, and
 * every request it makes has to carry the same project the router is rooted at
 * — deriving both from one function is what keeps them from ever disagreeing.
 */
export function projectFromPath(pathname: string): string | null {
  const parts = pathname.split("/").filter(Boolean);
  if (parts[0] !== "p" || !parts[1]) return null;
  setCurrentProject(parts[1]);
  return parts[1];
}

/** Where to send a browser that is switching projects, or has named none. */
export function projectHome(id: string): string {
  return `/p/${id}/`;
}

/**
 * `/` — send the browser to a project.
 *
 * The default one, asked of the server rather than remembered: an id in
 * `localStorage` would survive the project being deleted, and would be another
 * account's on a shared machine.
 *
 * Listing is also what *creates* an account's default project on its first
 * visit, so this is the one request that must happen before any scoped one.
 *
 * A `location.replace` rather than a `<Navigate>`: the destination is under a
 * different basename, so it is a new document rather than a route this router
 * knows about.
 */
export function ProjectRedirect() {
  const { data, isPending, error } = useProjects();

  if (isPending) return null;
  if (error || !data?.length) {
    return (
      <div className="p-8 text-sm text-faint">
        Could not load this account's projects.
      </div>
    );
  }
  const target = data.find((p) => p.isDefault) ?? data[0];
  window.location.replace(projectHome(target.id));
  return null;
}
