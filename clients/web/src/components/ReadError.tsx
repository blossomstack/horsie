import { useTranslation } from "react-i18next";
import { ApiRequestError } from "../api/client";
import { cn } from "../lib/cn";

/**
 * What a list shows in place of its contents when the read failed.
 *
 * A failed read used to be indistinguishable from an empty one: `data ?? []`
 * turned a dead server into "No cloud vendors are configured", and
 * `data?.length === 0` turned it into nothing at all. Both invite the same
 * wrong conclusion — that the thing you configured is gone — and neither
 * mentions the server. So: a failed read never renders as "you have none", and
 * never renders as silence.
 *
 * The panel markup is copied at a dozen sites already; this is the one that
 * knows what a *read* failure reads like, so a new list gets the wording right
 * by using it rather than by remembering the wording.
 */
export function ReadError({
  what,
  error,
  testId,
  className,
}: {
  /** What could not be loaded, already translated, as it would appear
   * mid-sentence: "skill bundles". */
  what: string;
  /** The query's own error, for the server's account of it. */
  error?: unknown;
  testId?: string;
  className?: string;
}) {
  const { t } = useTranslation();
  // Status 0 is the client's own "fetch threw" — its message already opens with
  // "Could not reach…", which reads as a stutter after "Couldn't load…".
  const detail =
    error instanceof ApiRequestError
      ? error.status === 0
        ? t("readError.unreachable")
        : error.message
      : t("readError.reload");
  return (
    <p
      role="status"
      data-testid={testId}
      className={cn(
        "rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2.5 text-sm leading-relaxed text-red-ink",
        className,
      )}
    >
      {t("readError.body", { what, detail })}
    </p>
  );
}
