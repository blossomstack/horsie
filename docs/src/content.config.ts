import { defineCollection, z } from "astro:content";
import { docsLoader } from "@astrojs/starlight/loaders";
import { docsSchema } from "@astrojs/starlight/schema";

/**
 * Every page declares exactly one Diátaxis kind, and must obey that kind's
 * rules — see `contributing/writing-docs`. Requiring it here means a page
 * cannot land without saying what it is: `astro build` rejects it.
 *
 * `description` is optional in Starlight's base schema. It is required here
 * because it is the page's meta description and its search snippet.
 */
export const collections = {
  docs: defineCollection({
    loader: docsLoader(),
    schema: docsSchema({
      extend: z.object({
        description: z.string(),
        kind: z.enum(["tutorial", "how-to", "reference", "explanation"]),
      }),
    }),
  }),
};
