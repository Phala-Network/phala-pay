import { createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { THEME_SCRIPT } from "../src/content/theme-script.ts";
import { PAGES, ROOT_MARKER, type Page } from "../src/content/template.ts";
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
  for (const page of Object.keys(PAGES) as Page[]) {
    const file = resolve(assets, PAGES[page].file);
    const template = await readFile(file, "utf8");
    await writeFile(file, renderPage(template, render(page), ROOT_MARKER));
  }
  const sitemap = `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${Object.values(PAGES).flatMap(({ path, lastmod }) => path === null ? [] : [`  <url><loc>${origin}${path}</loc><lastmod>${lastmod}</lastmod></url>`]).join("\n")}\n</urlset>\n`;
  await writeFile(resolve(assets, "sitemap.xml"), sitemap);
}

try {
  await prerender();
} catch (error) {
  console.error("Prerender failed:", error);
  process.exitCode = 1;
}
