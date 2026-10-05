import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { FAQ } from "../src/content/site.ts";

const web = resolve(import.meta.dirname, "..");
const assets = resolve(web, ".cloudflare/output/v0/workers/default/assets");
const ROOT_MARKER = '<div id="root"></div>';
const COMPARE_TITLE = "Crypto payment gateways compared for tokens | Phala Pay";
const COMPARE_DESCRIPTION = "How Phala Pay compares with Stripe, Coinbase Business, BTCPay Server, NOWPayments, and MoonPay Commerce on custody, fees, chains, speed, and refunds.";

async function prerender() {
  const { render } = await import(pathToFileURL(resolve(web, ".prerender/entry-server.js")).href) as typeof import("../src/entry-server.js");
  for (const page of ["home", "compare"] as const) {
    const file = resolve(assets, page === "home" ? "index.html" : "compare.html");
    let html = await readFile(file, "utf8");
    if (!html.includes(ROOT_MARKER)) throw new Error(`Missing root marker in ${file}`);
    const content = render(page);
    if (content.includes('style="')) throw new Error(`Inline style violates the CSP on ${page}`);
    html = html.replace(ROOT_MARKER, `<div id="root">${content}</div>`);
    if (page === "compare") {
      html = html.replace(/<title>[^<]*<\/title>/, `<title>${COMPARE_TITLE}</title>`)
        .replace(/(<meta\s+(?:name|property)="(?:description|og:description|twitter:description)"\s+content=")[^"]*("\s*\/>)/g, `$1${COMPARE_DESCRIPTION}$2`)
        .replace(/(<meta\s+(?:name|property)="(?:og:title|twitter:title)"\s+content=")[^"]*("\s*\/>)/g, `$1${COMPARE_TITLE}$2`)
        .replaceAll('href="https://pay.phala.com/"', 'href="https://pay.phala.com/compare"')
        .replace('content="https://pay.phala.com/"', 'content="https://pay.phala.com/compare"');
    }
    if (page === "home") {
      const faq = { "@context": "https://schema.org", "@type": "FAQPage", mainEntity: FAQ.map(({ question, answer }) => ({ "@type": "Question", name: question, acceptedAnswer: { "@type": "Answer", text: answer } })) };
      html = html.replace("</head>", `<script type="application/ld+json">${JSON.stringify(faq).replaceAll("<", "\\u003c")}</script>\n</head>`);
    }
    await writeFile(file, html);
  }
}

try {
  await prerender();
} catch (error) {
  console.error("Prerender failed:", error);
  process.exitCode = 1;
}
