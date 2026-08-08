
/**
 * One thing a bundle offers, as the settings page lists it and the composer
 * completes it.
 *
 * No template: the server expands an invocation from its own copy, so a client
 * never needs a command's body — and some of them run past a page.
 */
export interface CatalogEntryView {
  /**
   * `command`, `skill` or `agent`. Commands and skills are typed `/name`,
   * agents `@name`.
   */
  kind: string;
  name: string;
  description: string;
  /**
   * `argument-hint`, shown beside the name. Commands only.
   */
  argumentHint?: string;
}