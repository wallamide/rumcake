import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

// https://astro.build/config
export default defineConfig({
  base: "/rumcake",
  trailingSlash: "always",
  build: {
    format: "directory",
  },
  integrations: [
    starlight({
      title: "rumcake",
      lastUpdated: true,
      tableOfContents: {
        minHeadingLevel: 1,
      },
      social: {
        github: "https://github.com/Univa/rumcake",
      },
      pagination: false,
      sidebar: [
        {
          label: "Information",
          items: [
            {
              label: "Introduction",
              link: "/",
            },
          ],
        },
        {
          label: "Getting Started",
          autogenerate: { directory: "getting-started" },
        },
        {
          label: "Features",
          autogenerate: { directory: "features" },
        },
        {
          label: "API Reference",
          link: "https://github.com/Univa/rumcake",
        },
      ],
    }),
  ],
});
