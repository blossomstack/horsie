
/**
 * One thing a bundle offers, as the settings page lists it and the composer
 */
export interface CatalogEntryView {
  /**
   * `command`, `skill` or `agent`. Commands and skills are typed `/name`,
   */
  kind: string;
  name: string;
  description: string;
  /**
   * `argument-hint`, shown beside the name. Commands only.
   */
  argumentHint?: string;
}