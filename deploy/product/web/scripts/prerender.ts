import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parse, type DefaultTreeAdapterMap } from "parse5";
import { THEME_SCRIPT } from "../src/content/theme-script.ts";
import { ALL_PAGES, ENTRIES, ROOT_MARKER, route } from "../src/content/template.ts";
import { renderPage } from "./prerender-page.ts";

const web = resolve(import.meta.dirname, "..");
const assets = resolve(web, ".cloudflare/output/v0/workers/default/assets");
const origin = "https://pay.phala.com";

type Node = DefaultTreeAdapterMap["node"];

/** Every style attribute's value in a page, as the browser reads it (entities decoded). */
function styleAttributes(node: Node): string[] {
  const own = "attrs" in node ? node.attrs.filter(({ name }) => name === "style").map(({ value }) => value) : [];
  const children = "childNodes" in node ? node.childNodes : [];
  const content = "content" in node ? [node.content] : [];
  return [...own, ...[...children, ...content].flatMap(styleAttributes)];
}

/**
 * A page's style attributes (Radix's, in the prerendered demo) apply only where the CSP's
 * style-src-attr (else its style-src) permits each value by its hash, with 'unsafe-hashes': one it
 * does not would be dropped, and the element left unstyled once hydrated.
 */
function checkStyleAttributes(file: string, html: string, stylePolicy: string[]) {
  for (const value of new Set(styleAttributes(parse(html)))) {
    const hash = `'sha256-${createHash("sha256").update(value).digest("base64")}'`;
    if (!stylePolicy.includes("'unsafe-hashes'") || !stylePolicy.includes(hash)) {
      throw new Error(`${file}: the CSP does not permit style="${value}" (${hash}, with 'unsafe-hashes', in style-src-attr)`);
    }
  }
}

async function prerender() {
  const headers = await readFile(resolve(assets, "_headers"), "utf8");
  const themeHash = createHash("sha256").update(THEME_SCRIPT).digest("base64");
  // The policy's directives, from its header line (the file's comments may name them too).
  const policy = /^\s*Content-Security-Policy:\s*(.+)$/im.exec(headers)?.[1] ?? "";
  const directives = new Map(policy.split(";").map((directive) => directive.trim().split(/\s+/))
    .flatMap(([name, ...values]) => (name === undefined || name === "" ? [] : [[name.toLowerCase(), values] as const])));
  if (!directives.get("script-src")?.includes(`'sha256-${themeHash}'`)) throw new Error("Theme bootstrap CSP hash is stale");
  // The directive that governs style attributes: style-src-attr, falling back to style-src.
  const stylePolicy = directives.get("style-src-attr") ?? directives.get("style-src") ?? [];
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
    const rendered = renderPage(html, render(page), ROOT_MARKER);
    checkStyleAttributes(file, rendered, stylePolicy);
    await writeFile(resolve(assets, file), rendered);
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
