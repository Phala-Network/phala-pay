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
