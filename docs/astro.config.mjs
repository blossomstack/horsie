// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import starlightLinksValidator from "starlight-links-validator";

// https://astro.build/config
export default defineConfig({
  site: "https://docs.horsie.dev",
  integrations: [
    starlight({
      title: "horsie",
      description:
        "Documentation for horsie server — self-hosted LLM agent sessions in your browser.",
      favicon: "/favicon.svg",
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/blossomstack/horsie",
        },
      ],
      editLink: {
        baseUrl: "https://github.com/blossomstack/horsie/edit/main/docs/",
      },
      customCss: [
        "@fontsource-variable/archivo",
        "@fontsource-variable/martian-mono",
        "./src/styles/horsie.css",
      ],
      // Sections follow the reader's journey. Order *within* a section comes
      // from each page's `sidebar.order` frontmatter, which lint-prose.mjs
      // requires — so adding a page never means editing this list.
      sidebar: [
        {
          label: "Start here",
          items: [{ autogenerate: { directory: "start-here" } }],
        },
        {
          label: "Using horsie",
          items: [{ autogenerate: { directory: "using" } }],
        },
        {
          label: "Operating horsie",
          items: [{ autogenerate: { directory: "operating" } }],
        },
        { label: "CLI", items: [{ autogenerate: { directory: "cli" } }] },
        {
          label: "How it works",
          items: [{ autogenerate: { directory: "internals" } }],
        },
        {
          label: "Contributing",
          items: [{ autogenerate: { directory: "contributing" } }],
        },
      ],
      plugins: [
        // Fails the build on a dead internal link or a heading anchor that
        // does not exist. Renaming a page and leaving five links behind is the
        // most common way these docs rot.
        // `errorOnLocalLinks` is off because a page that tells you to open
        // http://localhost:3789 is giving an instruction, not navigating.
        starlightLinksValidator({
          errorOnRelativeLinks: false,
          errorOnLocalLinks: false,
        }),
      ],
      components: {
        // Adds a "back to horsie.dev" link beside the site title, so the docs
        // and the marketing site read as one property.
        SiteTitle: "./src/components/SiteTitle.astro",
      },
    }),
  ],
});
