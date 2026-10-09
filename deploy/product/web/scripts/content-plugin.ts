import { resolve } from "node:path";
import type { Plugin } from "vite";
import { DOCS } from "../src/content/docs.ts";
import { REPO_ROOT, renderDoc } from "./markdown.ts";
import { buildReference } from "./reference.ts";

/**
 * The site's rendered content as modules for the prerender only: `virtual:docs`, every doc of
 * src/content/docs.ts rendered from its markdown, and `virtual:reference`, the API reference's
 * model from crates/topup/openapi.json. Both are built when imported, so the markdown and the
 * OpenAPI document stay the source of truth; the client never loads them.
 */
export function contentPlugin(): Plugin {
  return {
    name: "phala-pay:content",
    resolveId(id) {
      return id === "virtual:docs" || id === "virtual:reference" ? `\0${id}` : null;
    },
    async load(id) {
      if (id === "\0virtual:docs") {
        for (const { file } of DOCS) this.addWatchFile(resolve(REPO_ROOT, file));
        // Each page's meta description is the one written for it in src/content/docs.ts.
        const docs = await Promise.all(DOCS.map(async ({ slug, file, description }) => ({ slug, file, ...(await renderDoc(file)), description })));
        return `export default ${JSON.stringify(docs)};`;
      }
      if (id === "\0virtual:reference") {
        this.addWatchFile(resolve(REPO_ROOT, "crates/topup/openapi.json"));
        return `export default ${JSON.stringify(await buildReference())};`;
      }
      return null;
    },
  };
}
