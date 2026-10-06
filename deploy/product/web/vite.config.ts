import { cloudflare } from "@cloudflare/vite-plugin";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";
import { defineConfig } from "vite";
import { ROOT_MARKER } from "./src/content/template.ts";
import { renderPage } from "./scripts/prerender-page.ts";

// The website, pay.phala.com: static pages at `/` and `/compare` with their assets in `assets/`, served by Cloudflare
// (cloudflare.config.ts, with the headers of public/_headers). The Cloudflare plugin writes the
// build as cf's Build Output, the page in .cloudflare/output/v0/workers/default/assets, and `vite
// preview` serves it in the Workers runtime. Its demo calls the reference product's API at
// VITE_DEMO_API_ORIGIN (.env.production). Content hashes in file names come from the content only,
// so two builds of the same sources are identical.
export default defineConfig({
  base: "./",
  plugins: [react(), tailwindcss(), cloudflare(), {
    name: "static-marketing-dev",
    apply: "serve",
    async transformIndexHtml(html, context) {
      if (context.server === undefined) return html;
      const { render } = await context.server.ssrLoadModule("/src/entry-server.tsx") as typeof import("./src/entry-server.js");
      const page = context.filename === resolve(import.meta.dirname, "compare.html") ? "compare" : "home";
      return renderPage(html, render(page), ROOT_MARKER);
    },
  }],
  resolve: { alias: { "@": resolve(import.meta.dirname, "src") } },
  build: {
    sourcemap: false,
    rollupOptions: { input: { home: resolve(import.meta.dirname, "index.html"), compare: resolve(import.meta.dirname, "compare.html") } },
  },
  logLevel: "warn",
});
