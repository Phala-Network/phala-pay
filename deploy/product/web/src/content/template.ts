import { SITE_UPDATED } from "./site.ts";
import { COMPARE_ACCESSED } from "./compare.ts";

// Shared Vite SSR template markers for development and build-time prerendering.
export const ROOT_MARKER = '<div id="root"></div>';
export const HEAD_MARKER = "<!--app-head-->";

/**
 * Static page routes, Vite entries, and sitemap content dates. The 404 page is what Cloudflare
 * serves, with a 404 status, for any path without an asset (cloudflare.config.ts); it has no
 * route of its own and stays out of the sitemap.
 */
export const PAGES = {
  home: { path: "/", file: "index.html", lastmod: SITE_UPDATED },
  compare: { path: "/compare", file: "compare.html", lastmod: COMPARE_ACCESSED },
  notFound: { path: null, file: "404.html", lastmod: null },
} as const;
export type Page = keyof typeof PAGES;
