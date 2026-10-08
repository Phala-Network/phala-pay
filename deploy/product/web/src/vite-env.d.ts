/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** The demo API's origin, without a trailing slash: https://pay-demo-api.phala.com. */
  readonly VITE_DEMO_API_ORIGIN: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}

/** A source file's text and its syntax tokens, highlighted at build time (scripts/highlight.ts). */
declare module "*?highlight" {
  const highlighted: import("../scripts/highlight.ts").HighlightedCode;
  export default highlighted;
}

/** The docs, rendered from the repository's markdown at build time (scripts/content-plugin.ts). */
declare module "virtual:docs" {
  const docs: import("./DocsPage.tsx").DocContent[];
  export default docs;
}

/** The API reference's model, built from crates/topup/openapi.json at build time. */
declare module "virtual:reference" {
  const reference: import("../scripts/reference.ts").ReferenceModel;
  export default reference;
}
