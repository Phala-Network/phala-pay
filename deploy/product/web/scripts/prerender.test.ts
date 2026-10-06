import { createHash } from "node:crypto";
import { THEME_SCRIPT } from "../src/content/theme-script.ts";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test, { after } from "node:test";
import { parse, type DefaultTreeAdapterMap } from "parse5";
import { createServer } from "vite";
import { pageMetadata, structuredData } from "../src/content/head.ts";
import { renderPage } from "./prerender-page.ts";
import { HEAD_MARKER, ROOT_MARKER } from "../src/content/template.ts";

type Node = DefaultTreeAdapterMap["node"];
type Element = DefaultTreeAdapterMap["element"];
function elements(node: Node): Element[] {
  const result = "tagName" in node ? [node] : [];
  if ("childNodes" in node) for (const child of node.childNodes) result.push(...elements(child));
  return result;
}
function attribute(node: Element, name: string): string | undefined {
  return node.attrs.find((attr) => attr.name === name)?.value;
}
function element(node: Node, tag: string, attributes: Record<string, string> = {}): Element {
  const result = elements(node).find((candidate) => candidate.tagName === tag
    && Object.entries(attributes).every(([name, value]) => attribute(candidate, name) === value));
  assert.ok(result, `Missing ${tag} element`);
  return result;
}
function text(node: Node): string {
  if ("value" in node) return node.value;
  return "childNodes" in node ? node.childNodes.map(text).join("") : "";
}

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
  const document = parse(renderPage(template, { head: renderHead(metadata, { description: value }), html: "<h1>Page</h1>" }, ROOT_MARKER));
  assert.equal(text(element(element(document, "div", { id: "root" }), "h1")), "Page");
  assert.equal(text(element(document, "title")), metadata.title);
  assert.equal(attribute(element(document, "link", { rel: "canonical" }), "href"), metadata.url);
  assert.equal(attribute(element(document, "meta", { name: "twitter:description" }), "content"), metadata.description);
  assert.equal(attribute(element(document, "meta", { property: "og:title" }), "content"), metadata.title);
  assert.equal(attribute(element(document, "meta", { name: "keywords" }), "content"), metadata.keywords);
  element(document, "script", { type: "module", src: "./src/main.tsx" });
  const scripts = elements(document).filter((node) => node.tagName === "script");
  assert.equal(scripts.length, 3);
  const json = text(element(document, "script", { type: "application/ld+json" }));
  const parsed: unknown = JSON.parse(json);
  assert.deepEqual(parsed, { description: value });
  assert.ok(!json.includes("<"));
});

await test("theme bootstrap is synchronous and authorized by the exact CSP hash", () => {
  const document = parse(renderPage(template, { head: renderHead(metadata, {}), html: "<h1>Page</h1>" }, ROOT_MARKER));
  const inlineScripts = elements(document).filter((node) => node.tagName === "script"
    && attribute(node, "src") === undefined && attribute(node, "type") !== "application/ld+json");
  assert.equal(inlineScripts.length, 1);
  const script = inlineScripts[0];
  assert.ok(script);
  assert.equal(text(script), THEME_SCRIPT);
  assert.equal(script.parentNode, element(document, "head"));
  assert.equal(attribute(script, "async"), undefined);
  assert.equal(attribute(script, "defer"), undefined);
  const headers = readFileSync(new URL("../public/_headers", import.meta.url), "utf8");
  const hash = createHash("sha256").update(THEME_SCRIPT).digest("base64");
  assert.ok(/\bscript-src\s+([^;\r\n]+)/i.exec(headers)?.[1]?.split(/\s+/).includes(`'sha256-${hash}'`));
});

await test("home and comparison head tags use their own content metadata", () => {
  for (const page of ["home", "compare"] as const) {
    const metadata = pageMetadata(page);
    const document = parse(`<head>${renderHead(metadata, structuredData(page))}</head>`);
    assert.equal(text(element(document, "title")), metadata.title);
    assert.equal(attribute(element(document, "meta", { name: "twitter:image" }), "content"),
      attribute(element(document, "meta", { property: "og:image" }), "content"));
    assert.equal(attribute(element(document, "link", { rel: "canonical" }), "href"), metadata.url);
    assert.equal(attribute(element(document, "meta", { name: "keywords" }), "content"), metadata.keywords);
  }
});
