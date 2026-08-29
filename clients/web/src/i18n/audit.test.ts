import { describe, expect, it } from "vitest";
import { audit } from "../../scripts/i18n-audit.mjs";

/**
 * The scanner, as a gate.
 *
 * It walks `src/` rather than checking a list of files that are "done", so a
 * page added tomorrow with a hard-coded string fails here without anyone
 * having to register it — which is the only version of this check that keeps
 * working after the people who wrote it have moved on.
 */
describe("i18n audit", () => {
  it("finds no hard-coded strings and no unknown keys", () => {
    const findings = audit().map(
      (f) => `${f.rel}:${f.line} [${f.kind}] ${f.text}`,
    );
    expect(findings).toEqual([]);
  });
});
