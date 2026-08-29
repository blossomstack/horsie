import "i18next";
import type en from "./locales/en";

/** Makes `t()` reject a key no catalogue has. Without it every typo is a
 * silent passthrough of the key itself, which renders as `settings.tilte`
 * on screen and passes every check we run. */
declare module "i18next" {
  interface CustomTypeOptions {
    defaultNS: "translation";
    resources: { translation: typeof en };
  }
}
