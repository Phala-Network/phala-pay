import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { THEME_SCRIPT } from "../src/content/theme-script.ts";
import { ALL_PAGES, ENTRIES, ROOT_MARKER, route } from "../src/content/template.ts";
import { renderPage } from "./prerender-page.ts";

const web = resolve(import.meta.dirname, "..");
const assets = resolve(web, ".cloudflare/output/v0/workers/default/assets");
const origin = "https://pay.phala.com";

async function prerender() {
  const headers = await readFile(resolve(assets, "_headers"), "utf8");
  const themeHash = createHash("sha256").update(THEME_SCRIPT).digest("base64");
  const scriptPolicy = /\bscript-src\s+([^;\r\n]+)/i.exec(headers)?.[1];
  if (!scriptPolicy?.split(/\s+/).includes(`'sha256-${themeHash}'`)) throw new Error("Theme bootstrap CSP hash is stale");
  const { render } = await import(pathToFileURL(resolve(web, ".prerender/entry-server.js")).href) as typeof import("../src/entry-server.js");
  // Every template is read before any page is written: a template's own file is also a page (the
  // docs' index is docs.html).
  const templates = new Map(await Promise.all(Object.values(ENTRIES).map(async (template) =>
    [template, await readFile(resolve(assets, template), "utf8")] as const)));
  for (const page of ALL_PAGES) {
    const { template, file } = route(page);
    const html = templates.get(template);
    if (html === undefined) throw new Error(`No built template ${template} for ${page}`);
    await mkdir(dirname(resolve(assets, file)), { recursive: true });
    await writeFile(resolve(assets, file), renderPage(html, render(page), ROOT_MARKER));
  }
  const urls = ALL_PAGES.map(route).flatMap(({ path, lastmod }) => path === null ? [] :
    [`  <url><loc>${origin}${path}</loc>${lastmod === null ? "" : `<lastmod>${lastmod}</lastmod>`}</url>`]);
  const sitemap = `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${urls.join("\n")}\n</urlset>\n`;
  await writeFile(resolve(assets, "sitemap.xml"), sitemap);
}

try {
  await prerender();
} catch (error) {
  console.error("Prerender failed:", error);
  process.exitCode = 1;
}
