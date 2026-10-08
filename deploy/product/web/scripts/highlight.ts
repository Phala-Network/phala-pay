import { readFile } from "node:fs/promises";
import { extname } from "node:path";
import { createCssVariablesTheme, createHighlighter, type BundledLanguage, type Highlighter } from "shiki";
import type { Plugin } from "vite";

/**
 * Syntax highlighting at build time, with Shiki's CSS variables theme: each token's colour is one
 * of the theme's variables (`var(--shiki-token-keyword)`), which becomes a class (`tok-keyword`)
 * that src/index.css colours. Pages carry no inline style (the CSP's style-src) and no highlighter
 * code.
 */
const theme = createCssVariablesTheme({ name: "phala-pay", variablePrefix: "--shiki-", fontStyle: false });

/** The languages the site's code is written in; anything else is shown unhighlighted. */
export const LANGUAGES = ["ts", "tsx", "js", "json", "sh", "bash", "python", "toml", "yaml", "http", "rust", "sql", "diff"] as const;

let highlighter: Promise<Highlighter> | undefined;
export function getHighlighter(): Promise<Highlighter> {
  highlighter ??= createHighlighter({ themes: [theme], langs: [...LANGUAGES] });
  return highlighter;
}

/** A token's text and its class (none for the plain foreground). */
export interface Token { text: string; className?: string }
/** Code and its lines of tokens. */
export interface HighlightedCode { code: string; lines: Token[][] }

/** `var(--shiki-token-keyword)` → `tok-keyword`; the foreground has no class. */
export function tokenClass(color: string | undefined): string | undefined {
  const name = /^var\(--shiki-token-([a-z-]+)\)$/.exec(color ?? "")?.[1];
  return name === undefined ? undefined : `tok-${name}`;
}

export function isLanguage(lang: string): lang is (typeof LANGUAGES)[number] & BundledLanguage {
  return (LANGUAGES as readonly string[]).includes(lang);
}

export async function tokenize(code: string, lang: string): Promise<Token[][]> {
  if (!isLanguage(lang)) return code.split("\n").map((line) => [{ text: line }]);
  const { tokens } = (await getHighlighter()).codeToTokens(code, { lang, theme: "phala-pay" });
  // Neighbouring tokens of one colour become one, and whitespace joins its neighbour: fewer
  // elements on the page for the same text.
  return tokens.map((line) => line.reduce<Token[]>((merged, { content, color }) => {
    const className = /^\s+$/.test(content) ? merged.at(-1)?.className : tokenClass(color);
    const last = merged.at(-1);
    if (last !== undefined && last.className === className) last.text += content;
    else merged.push(className === undefined ? { text: content } : { text: content, className });
    return merged;
  }, []));
}

const SUFFIX = "?highlight";
const PREFIX = "\0highlight:";

/**
 * `import snippet from "./file.ts?highlight"`: the file's text and its tokens, as a JSON module
 * (`HighlightedCode`). The language is the file's extension.
 */
export function highlightPlugin(): Plugin {
  return {
    name: "phala-pay:highlight",
    enforce: "pre",
    async resolveId(source, importer) {
      if (!source.endsWith(SUFFIX)) return null;
      const resolved = await this.resolve(source.slice(0, -SUFFIX.length), importer, { skipSelf: true });
      return resolved === null ? null : `${PREFIX}${resolved.id}`;
    },
    async load(id) {
      if (!id.startsWith(PREFIX)) return null;
      const file = id.slice(PREFIX.length);
      this.addWatchFile(file);
      const code = (await readFile(file, "utf8")).trimEnd();
      const highlighted: HighlightedCode = { code, lines: await tokenize(code, extname(file).slice(1)) };
      return `export default ${JSON.stringify(highlighted)};`;
    },
  };
}
