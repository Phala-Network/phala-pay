import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { renderPage } from "./prerender-page.ts";

const template = readFileSync(new URL("../index.html", import.meta.url), "utf8");
const metadata = { title: 'Tokens & "treasury"', description: "A < B", url: "https://pay.phala.com/compare" };

await test("missing mount marker fails the build instead of publishing an empty shell", () => {
  assert.throws(() => renderPage(template.replace('<div id="root"></div>', ""), "<h1>Page</h1>", metadata, {}), /Missing root marker/);
});

await test("prerender rejects styles that the production CSP would block", () => {
  assert.throws(() => renderPage(template, '<h1 style="color:red">Page</h1>', metadata, {}), /Inline style/);
});

await test("page metadata and JSON-LD are escaped without changing Vite's client entry", () => {
  const value = '</script><script src="evil"></script>';
  const html = renderPage(template, "<h1>Page</h1>", metadata, { description: value });
  assert.ok(html.includes('<div id="root"><h1>Page</h1></div>'));
  assert.ok(html.includes("<title>Tokens &amp; &quot;treasury&quot;</title>"));
  assert.ok(html.includes('href="https://pay.phala.com/compare"'));
  assert.ok(html.includes('name="twitter:description" content="A &lt; B"'));
  assert.ok(html.includes('property="og:title" content="Tokens &amp; &quot;treasury&quot;"'));
  assert.ok(html.includes('src="./src/main.tsx"'));
  const json = /<script type="application\/ld\+json">(.*?)<\/script>/.exec(html)?.[1];
  assert.ok(json);
  const parsed: unknown = JSON.parse(json);
  assert.deepEqual(parsed, { description: value });
  assert.ok(!json.includes("<"));
});
