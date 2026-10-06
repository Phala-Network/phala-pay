import { createHash } from "node:crypto";
import { THEME_SCRIPT } from "../src/content/theme-script.ts";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test, { after } from "node:test";
import { createServer } from "vite";
import { pageMetadata, structuredData } from "../src/content/head.ts";
import { renderPage } from "./prerender-page.ts";
import { HEAD_MARKER, ROOT_MARKER } from "../src/content/template.ts";

const template = readFileSync(new URL("../index.html", import.meta.url), "utf8");
const server = await createServer({ configFile: new URL("../vite.ssr.config.ts", import.meta.url).pathname, server: { middlewareMode: true, watch: null } });
after(() => server.close());
const { renderHead } = await server.ssrLoadModule("/src/Head.tsx") as typeof import("../src/Head.js");
const metadata = { title: 'Tokens & "treasury"', description: "A < B", keywords: "gateways & tokens", url: "https://pay.phala.com/compare" };

await test("missing mount or head marker fails the build instead of publishing an empty shell", () => {
  const rendered = { head: "<title>Page</title>", html: "<h1>Page</h1>" };
  assert.throws(() => renderPage(template.replace(ROOT_MARKER, ""), rendered, ROOT_MARKER), /Missing root marker/);
  assert.throws(() => renderPage(template.replace(HEAD_MARKER, ""), rendered, ROOT_MARKER), /Missing head marker/);
});

await test("React escapes metadata and JSON-LD without changing Vite's client entry", () => {
  const value = '</script><script src="evil"></script>';
  const html = renderPage(template, { head: renderHead(metadata, { description: value }), html: "<h1>Page</h1>" }, ROOT_MARKER);
  assert.ok(html.includes('<div id="root"><h1>Page</h1></div>'));
  assert.ok(html.includes("<title>Tokens &amp; &quot;treasury&quot;</title>"));
  assert.ok(html.includes('href="https://pay.phala.com/compare"'));
  assert.ok(html.includes('name="twitter:description" content="A &lt; B"'));
  assert.ok(html.includes('property="og:title" content="Tokens &amp; &quot;treasury&quot;"'));
  assert.ok(html.includes('src="./src/main.tsx"'));
  assert.ok(html.includes('name="keywords" content="gateways &amp; tokens"'));
  const json = /<script\b\s+type\s*=\s*"application\/ld\+json"\s*>([\s\S]*?)<\/script\s*>/i.exec(html)?.[1];
  assert.ok(json);
  const parsed: unknown = JSON.parse(json);
  assert.deepEqual(parsed, { description: value });
  assert.ok(!json.includes("<"));
});

await test("theme bootstrap is synchronous and authorized by the exact CSP hash", () => {
  const html = renderPage(template, { head: renderHead(metadata, {}), html: "<h1>Page</h1>" }, ROOT_MARKER);
  assert.equal(executableInlineScriptTags(html).length, 1);
  assert.ok(html.includes(`<script>${THEME_SCRIPT}</script>`));
  assert.ok(html.indexOf(`<script>${THEME_SCRIPT}</script>`) < html.indexOf("</head>"));
  const headers = readFileSync(new URL("../public/_headers", import.meta.url), "utf8");
  const hash = createHash("sha256").update(THEME_SCRIPT).digest("base64");
  assert.ok(/\bscript-src\s+([^;\r\n]+)/i.exec(headers)?.[1]?.split(/\s+/).includes(`'sha256-${hash}'`));
});

// Count opening tags independently of closing-tag spelling or script contents.
function executableInlineScriptTags(html: string): string[] {
  return [...html.matchAll(/<script\b[^>]*>/gi)]
    .map((match) => match[0])
    .filter((tag) => !/\ssrc\s*=/i.test(tag)
      && !/\stype\s*=\s*(?:"application\/ld\+json"|'application\/ld\+json'|application\/ld\+json(?=\s|>))/i.test(tag));
}

await test("script detection counts uppercase and multiline tags with spaced attributes", () => {
  const html = `<SCRIPT nonce="test">first()</SCRIPT>
<ScRiPt
  defer
>second()</sCrIpT >
<script type="module">third()</script>
<SCRIPT SRC = "/entry.js"></SCRIPT>
<script TYPE = 'application/ld+json'>{}</script>
<script type = application/ld+json>{}</script>
<scripted>Not a script element</scripted>`;
  assert.deepEqual(executableInlineScriptTags(html), [
    '<SCRIPT nonce="test">',
    '<ScRiPt\n  defer\n>',
    '<script type="module">',
  ]);
});

await test("home and comparison head tags use their own content metadata", () => {
  for (const page of ["home", "compare"] as const) {
    const metadata = pageMetadata(page);
    const head = renderHead(metadata, structuredData(page));
    assert.ok(head.includes(`<title>${metadata.title}</title>`));
    assert.ok(head.includes(`href="${metadata.url}"`));
    assert.ok(head.includes(`content="${metadata.keywords}"`));
  }
});
