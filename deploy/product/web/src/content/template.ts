import { SITE_UPDATED } from "./site.ts";
import { COMPARE_ACCESSED } from "./compare.ts";
import { DOCS, docPath } from "./docs.ts";

// Shared Vite SSR template markers for development and build-time prerendering.
export const ROOT_MARKER = '<div id="root"></div>';
export const HEAD_MARKER = "<!--app-head-->";

/** A page's route: where the site serves it, the HTML template it is rendered into, and its file. */
export interface PageRoute { path: string | null; template: string; file: string; lastmod: string | null }

/**
 * The fixed pages, with their sitemap content dates. The 404 page is what Cloudflare serves, with a
 * 404 status, for any path without an asset (cloudflare.config.ts); it has no route of its own and
 * stays out of the sitemap.
 */
export const PAGES = {
  home: { path: "/", template: "index.html", file: "index.html", lastmod: SITE_UPDATED },
  compare: { path: "/compare", template: "compare.html", file: "compare.html", lastmod: COMPARE_ACCESSED },
  notFound: { path: null, template: "404.html", file: "404.html", lastmod: null },
  reference: { path: "/reference", template: "reference.html", file: "reference.html", lastmod: null },
} as const satisfies Record<string, PageRoute>;
export type StaticPage = keyof typeof PAGES;
/** A fixed page, or a doc by its slug (src/content/docs.ts). */
export type Page = StaticPage | `doc:${string}`;

/** Each doc renders into docs.html: `/docs` is docs.html itself, `/docs/<slug>` docs/<slug>.html. */
const DOCS_TEMPLATE = "docs.html";

export function isStaticPage(page: Page): page is StaticPage {
  return page in PAGES;
}

export function route(page: Page): PageRoute {
  if (isStaticPage(page)) return PAGES[page];
  const slug = page.slice("doc:".length);
  return { path: docPath(slug), template: DOCS_TEMPLATE, file: slug === "" ? DOCS_TEMPLATE : `docs/${slug}.html`, lastmod: null };
}

export const ALL_PAGES: Page[] = [...(Object.keys(PAGES) as StaticPage[]), ...DOCS.map(({ slug }) => `doc:${slug}` as const)];

/** Vite's entries: one per template, named as its page (the docs' by `docs`). */
export const ENTRIES: Record<string, string> = {
  ...Object.fromEntries(Object.entries(PAGES).map(([page, { template }]) => [page, template])),
  docs: DOCS_TEMPLATE,
};
