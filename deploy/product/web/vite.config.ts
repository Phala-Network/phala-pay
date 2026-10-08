import { cloudflare } from "@cloudflare/vite-plugin";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { basename, resolve } from "node:path";
import { defineConfig } from "vite";
import { ALL_PAGES, ENTRIES, ROOT_MARKER, route } from "./src/content/template.ts";
import { contentPlugin } from "./scripts/content-plugin.ts";
import { highlightPlugin } from "./scripts/highlight.ts";
import { renderPage } from "./scripts/prerender-page.ts";

// The website, pay.phala.com: static pages (`/`, `/compare`, `/docs`, `/reference`) with their assets in `assets/`, served by Cloudflare
// (cloudflare.config.ts, with the headers of public/_headers). The Cloudflare plugin writes the
// build as cf's Build Output, the page in .cloudflare/output/v0/workers/default/assets, and `vite
// preview` serves it in the Workers runtime. Its demo calls the reference product's API at
// VITE_DEMO_API_ORIGIN (.env.production). Content hashes in file names come from the content only,
// so two builds of the same sources are identical. Cloudflare asset routing also returns 404 for
// unknown development pages; sitemap.xml exists only in build output.
export default defineConfig({
  // Absolute asset URLs: the 404 page is served at any unmatched path, however deep.
  base: "/",
  plugins: [highlightPlugin(), contentPlugin(), react(), tailwindcss(), cloudflare(), {
    name: "static-marketing-dev",
    apply: "serve",
    async transformIndexHtml(html, context) {
      if (context.server === undefined) return html;
      const { render } = await context.server.ssrLoadModule("/src/entry-server.tsx") as typeof import("./src/entry-server.js");
      // A template renders its first page: docs.html the docs' index.
      const page = ALL_PAGES.find((key) => route(key).template === basename(context.filename));
      if (page === undefined) throw new Error(`Unknown page template: ${context.filename}`);
      return renderPage(html, render(page), ROOT_MARKER);
    },
  }],
  resolve: { alias: { "@": resolve(import.meta.dirname, "src") } },
  build: {
    sourcemap: false,
    rollupOptions: { input: Object.fromEntries(Object.entries(ENTRIES).map(([name, template]) => [name, resolve(import.meta.dirname, template)])) },
  },
  logLevel: "warn",
});
