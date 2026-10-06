import { defineConfig } from "cf/config";

// pay.phala.com: the page built from this directory, served by a Worker with static assets only (no
// Worker script). The Cloudflare Vite plugin (vite.config.ts) builds the page's assets into cf's
// Build Output. Cloudflare Workers Builds deploys it from this repository (deploy/phala.md,
// "Website"): `main` to production, and every other branch to its own Preview (`isPreview`), which
// Cloudflare serves on workers.dev. Its demo API calls fail by design: the API allows only
// https://pay.phala.com.
export default defineConfig(({ isPreview }) => ({
  worker: {
    name: "phala-pay-web",
    compatibilityDate: "2026-09-26",
    assets: {
      // Static pages at `/` and `/compare` with their assets: unmatched paths return a real 404,
      // instead of a page fallback (which "single-page-application" would serve).
      notFoundHandling: "none",
    },
    // Production only: a Preview cannot have custom domains.
    ...(isPreview ? {} : { domains: ["pay.phala.com"] }),
    // The site is served only on its custom domain: no second copy at *.workers.dev.
    workersDev: false,
    // On for production too: a branch's Preview gets workers.dev URLs only when the Worker itself has
    // preview URLs enabled. The cost is a workers.dev URL per production version, which serves the
    // same public page, marked noindex (public/_headers).
    previewUrls: true,
  },
}));
