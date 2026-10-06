import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { COMPARE_ACCESSED } from "../src/content/compare.ts";
import { THEME_SCRIPT } from "../src/content/theme-script.ts";
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
  for (const page of ["home", "compare"] as const) {
    const file = resolve(assets, page === "home" ? "index.html" : "compare.html");
    const template = await readFile(file, "utf8");
    await writeFile(file, renderPage(template, render(page)));
  }
  const homeLastmod = execFileSync("git", ["log", "-1", "--format=%cs", "--", "src/content/site.ts"], { cwd: web, encoding: "utf8" }).trim();
  if (!/^\d{4}-\d{2}-\d{2}$/.test(homeLastmod)) throw new Error("Missing content commit date for sitemap");
  const sitemap = `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${[["/", homeLastmod], ["/compare", COMPARE_ACCESSED]].map(([path, lastmod]) => `  <url><loc>${origin}${path}</loc><lastmod>${lastmod}</lastmod></url>`).join("\n")}\n</urlset>\n`;
  await writeFile(resolve(assets, "sitemap.xml"), sitemap);
}

try {
  await prerender();
} catch (error) {
  console.error("Prerender failed:", error);
  process.exitCode = 1;
}
