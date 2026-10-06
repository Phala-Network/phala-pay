import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { COMPARE_ACCESSED, COMPARE_DESCRIPTION, COMPARE_KEYWORDS, COMPARE_TITLE } from "../src/content/compare.ts";
import { FAQ, HOME_DESCRIPTION, HOME_TITLE } from "../src/content/site.ts";
import { THEME_SCRIPT } from "../src/content/theme-script.ts";
import { renderPage } from "./prerender-page.ts";

const web = resolve(import.meta.dirname, "..");
const assets = resolve(web, ".cloudflare/output/v0/workers/default/assets");
const origin = "https://pay.phala.com";
const repo = "https://github.com/Phala-Network/phala-pay";

async function prerender() {
  const headers = await readFile(resolve(assets, "_headers"), "utf8");
  const themeHash = createHash("sha256").update(THEME_SCRIPT).digest("base64");
  const scriptPolicy = /script-src ([^;]+)/.exec(headers)?.[1];
  if (!scriptPolicy?.split(/\s+/).includes(`'sha256-${themeHash}'`)) throw new Error("Theme bootstrap CSP hash is stale");
  const { render } = await import(pathToFileURL(resolve(web, ".prerender/entry-server.js")).href) as typeof import("../src/entry-server.js");
  // The web shell has no release version; use the lockstep service/SDK package version.
  const pkg: unknown = JSON.parse(await readFile(resolve(web, "../../../sdk/js/package.json"), "utf8"));
  if (typeof pkg !== "object" || pkg === null || !("version" in pkg) || typeof pkg.version !== "string") {
    throw new Error("SDK package has no software version");
  }
  const organization = {
    "@type": "Organization", "@id": `${origin}/#organization`, name: "Phala Network", url: "https://phala.com",
    logo: "https://phala.com/home/logo.svg", sameAs: ["https://github.com/Phala-Network"],
  };
  const website = {
    "@type": "WebSite", "@id": `${origin}/#website`, name: "Phala Pay", url: `${origin}/`,
    description: HOME_DESCRIPTION, publisher: { "@id": `${origin}/#organization` },
  };
  for (const page of ["home", "compare"] as const) {
    const file = resolve(assets, page === "home" ? "index.html" : "compare.html");
    const template = await readFile(file, "utf8");
    const home = page === "home";
    const metadata = { ...(home ? {} : { keywords: COMPARE_KEYWORDS }), title: home ? HOME_TITLE : COMPARE_TITLE, description: home ? HOME_DESCRIPTION : COMPARE_DESCRIPTION, url: home ? `${origin}/` : `${origin}/compare` };
    const graph = { "@context": "https://schema.org", "@graph": home ? [
      organization, website,
      {
        "@type": "SoftwareApplication", name: "Phala Pay", url: `${origin}/`, applicationCategory: "FinanceApplication",
        softwareVersion: pkg.version, license: `${repo}/blob/main/LICENSE`, offers: { "@type": "Offer", price: "0", priceCurrency: "USD" },
        publisher: { "@id": `${origin}/#organization` }, description: HOME_DESCRIPTION,
      },
      { "@type": "SoftwareSourceCode", name: "Phala Pay", codeRepository: repo, license: `${repo}/blob/main/LICENSE` },
      { "@type": "FAQPage", mainEntity: FAQ.map(({ question, answer }) => ({ "@type": "Question", name: question, acceptedAnswer: { "@type": "Answer", text: answer } })) },
    ] : [
      organization, website,
      { "@type": "WebPage", "@id": `${origin}/compare#webpage`, url: metadata.url, name: metadata.title, description: metadata.description, dateModified: COMPARE_ACCESSED, isPartOf: { "@id": `${origin}/#website` }, breadcrumb: { "@id": `${origin}/compare#breadcrumb` } },
      { "@type": "BreadcrumbList", "@id": `${origin}/compare#breadcrumb`, itemListElement: [
        { "@type": "ListItem", position: 1, name: "Phala Pay", item: `${origin}/` },
        { "@type": "ListItem", position: 2, name: "Compare", item: `${origin}/compare` },
      ] },
    ] };
    await writeFile(file, renderPage(template, render(page), metadata, graph));
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
