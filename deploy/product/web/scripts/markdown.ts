import { readFile } from "node:fs/promises";
import { posix, resolve } from "node:path";
import type { Element, ElementContent, Root } from "hast";
import { toString } from "hast-util-to-string";
import rehypeAutolinkHeadings from "rehype-autolink-headings";
import rehypeSlug from "rehype-slug";
import rehypeStringify from "rehype-stringify";
import remarkGfm from "remark-gfm";
import remarkParse from "remark-parse";
import remarkRehype, { type Options as RemarkRehypeOptions } from "remark-rehype";
import { unified, type PluggableList } from "unified";
import { visit } from "unist-util-visit";
import { DOCS, docPath } from "../src/content/docs.ts";
import { tokenize } from "./highlight.ts";

/**
 * The site's markdown, rendered at build time: GitHub-flavoured markdown (tables, task lists,
 * autolinks); headings with GitHub's anchors (rehype-slug uses github-slugger, so every
 * `file.md#section` link keeps working) and a link to themselves; code highlighted by the site's
 * highlighter (classes, no inline styles); and links rewritten for the site: to another rendered
 * doc, its page here; to the old API reference, /reference; to any other file in the repository,
 * that file on GitHub. Raw HTML in the markdown is dropped (remark-rehype's default), so a page
 * carries nothing the CSP would refuse, save one form the docs use as a link target: an empty
 * `<a id="…"></a>`, kept as an empty element with that id.
 */
export const REPO_ROOT = resolve(import.meta.dirname, "../../../..");
const REPO = "https://github.com/Phala-Network/phala-pay";
const OLD_REFERENCE = /^https:\/\/phala-network\.github\.io\/phala-pay\/?(#.*)?$/;

export interface TocEntry { depth: 2 | 3; id: string; text: string }
export interface RenderedDoc { title: string; description: string; html: string; toc: TocEntry[] }

const pathOfFile = new Map(DOCS.map(({ file, slug }) => [file, docPath(slug)]));

/** A link in the repository file `file`, as the site serves it. */
export function siteHref(href: string, file: string): string {
  const reference = OLD_REFERENCE.exec(href);
  if (reference !== null) return `/reference${reference[1] ?? ""}`;
  // A GitHub link to a doc this site renders (the OpenAPI document links that way) comes here too.
  const blob = `${REPO}/blob/main/`;
  if (href.startsWith(blob)) {
    const [path = "", hash] = href.slice(blob.length).split("#", 2);
    const page = pathOfFile.get(path);
    if (page !== undefined) return hash === undefined ? page : `${page}#${hash}`;
  }
  if (/^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith("#") || href.startsWith("//")) return href;
  const [path = "", hash] = href.split("#", 2);
  const target = posix.normalize(posix.join(posix.dirname(file), path));
  const page = pathOfFile.get(target);
  if (page !== undefined) return hash === undefined ? page : `${page}#${hash}`;
  // A directory (no extension) opens as a tree on GitHub, a file as a blob.
  const kind = posix.extname(target) === "" ? "tree" : "blob";
  return `${REPO}/${kind}/main/${target}${hash === undefined ? "" : `#${hash}`}`;
}

const text = (value: string): ElementContent => ({ type: "text", value });
const element = (tagName: string, properties: Element["properties"], children: ElementContent[]): Element =>
  ({ type: "element", tagName, properties, children });

/**
 * Each code block, highlighted and framed: a header with its language and a copy button (hidden
 * until src/docs-main.tsx wires it up), then the code, focusable so the keyboard scrolls it. A
 * Mermaid diagram stays as its source, with a link to GitHub, which renders it: drawing it here
 * would take a browser at build time or a diagram library on the page.
 */
function rehypeCodeBlocks(file: string) {
  return async (tree: Root) => {
    const blocks: { parent: Element | Root; index: number; pre: Element; code: Element; lang: string }[] = [];
    visit(tree, "element", (node: Element, index, parent) => {
      if (node.tagName !== "pre" || index === undefined || parent === undefined) return;
      const code = node.children.find((child): child is Element => child.type === "element" && child.tagName === "code");
      if (code === undefined) return;
      const classes = Array.isArray(code.properties.className) ? code.properties.className.map(String) : [];
      const lang = classes.find((name) => name.startsWith("language-"))?.slice("language-".length) ?? "";
      blocks.push({ parent, index, pre: node, code, lang });
    });
    for (const { parent, index, pre, code, lang } of blocks.reverse()) {
      const source = toString(code).replace(/\n$/, "");
      const lines = await tokenize(source, lang === "typescript" ? "ts" : lang);
      code.children = lines.map((line) => element("span", { className: ["line"] }, line.map((token) =>
        token.className === undefined ? text(token.text) : element("span", { className: [token.className] }, [text(token.text)]))));
      pre.properties = { tabIndex: 0 };
      const header = element("div", { className: ["code-block-header"] }, [
        element("span", {}, [text(lang === "" ? "text" : lang)]),
        element("button", { type: "button", className: ["code-block-copy"], dataCopy: "", ariaLive: "polite", hidden: true }, [text("Copy")]),
      ]);
      const frame: ElementContent[] = [header, pre];
      if (lang === "mermaid") {
        frame.push(element("p", { className: ["code-block-note"] }, [
          text("A Mermaid diagram's source. "),
          element("a", { href: `${REPO}/blob/main/${file}` }, [text("See it drawn on GitHub")]),
          text("."),
        ]));
      }
      parent.children.splice(index, 1, element("div", { className: ["code-block"], dataLanguage: lang }, frame));
    }
  };
}

/**
 * Links rewritten for the site; and a task list's checkbox (read-only, as GitHub renders it) named by
 * its item's text, which it otherwise only sits beside.
 */
function rehypeLinks(file: string) {
  return (tree: Root) => {
    visit(tree, "element", (node: Element) => {
      if (node.tagName === "a" && typeof node.properties.href === "string") node.properties.href = siteHref(node.properties.href, file);
      if (node.tagName === "li" && Array.isArray(node.properties.className) && node.properties.className.includes("task-list-item")) {
        const box = node.children.find((child): child is Element => child.type === "element" && child.tagName === "input");
        const label = node.children.filter((child) => !(child.type === "element" && /^(ul|ol)$/.test(child.tagName)));
        if (box !== undefined) box.properties.ariaLabel = toString({ type: "root", children: label }).trim();
      }
    });
  };
}

/** The page's title (its H1, removed from the body: the page sets it), outline, and first paragraph. */
/**
 * Each table, ready to stack on a phone (src/index.css): every cell carries its column's header
 * as `data-label`, shown above it once rows become blocks, and the table keeps its roles
 * explicitly, as a table restyled with `display: block` can lose them.
 */
function rehypeTables() {
  return (tree: Root) => {
    visit(tree, "element", (table: Element) => {
      if (table.tagName !== "table") return;
      table.properties.role = "table";
      const headers: string[] = [];
      visit(table, "element", (node: Element) => {
        if (node.tagName === "thead" || node.tagName === "tbody") node.properties.role = "rowgroup";
        if (node.tagName === "tr") node.properties.role = "row";
        if (node.tagName === "th") {
          node.properties.role = "columnheader";
          headers.push(toString(node).trim());
        }
      });
      // An identifier in a cell may wrap where it divides (`TOPUP_ADMIN_` / `PUBLIC_KEY`), and
      // nowhere else: a word-break opportunity after each separator, so a column is never squeezed
      // to letters.
      visit(table, "element", (code: Element) => {
        if (code.tagName !== "code") return;
        code.children = code.children.flatMap((child) => child.type !== "text" ? [child]
          : child.value.split(/(?<=[_./:(,=|])/).flatMap((part, index): ElementContent[] =>
            index === 0 ? [text(part)] : [element("wbr", {}, []), text(part)]));
      });
      visit(table, "element", (row: Element) => {
        if (row.tagName !== "tr") return;
        row.children
          .filter((child): child is Element => child.type === "element" && child.tagName === "td")
          .forEach((cell, index) => {
            cell.properties.role = "cell";
            const label = headers[index];
            if (label !== undefined && label !== "") cell.properties.dataLabel = label;
          });
      });
    });
  };
}

function rehypeOutline(outline: { title: string; description: string; toc: TocEntry[] }) {
  return (tree: Root) => {
    tree.children = tree.children.filter((node) => {
      if (node.type !== "element" || node.tagName !== "h1" || outline.title !== "") return true;
      outline.title = toString(node).trim();
      return false;
    });
    visit(tree, "element", (node: Element) => {
      if ((node.tagName === "h2" || node.tagName === "h3") && typeof node.properties.id === "string") {
        outline.toc.push({ depth: node.tagName === "h2" ? 2 : 3, id: node.properties.id, text: toString(node).trim() });
      }
      if (node.tagName === "p" && outline.description === "") outline.description = toString(node).replace(/\s+/g, " ").trim();
    });
  };
}

/** At most 160 characters, cut at a word, for a meta description. */
export function summarize(value: string): string {
  if (value.length <= 160) return value;
  const cut = value.slice(0, 157);
  return `${cut.slice(0, cut.lastIndexOf(" "))}…`;
}

const anchors = {
  behavior: "append",
  test: ["h2", "h3", "h4"],
  properties: { className: ["heading-anchor"], ariaHidden: "true", tabIndex: -1 },
  content: text("#"),
} as const;

type HtmlHandler = NonNullable<NonNullable<RemarkRehypeOptions["handlers"]>["html"]>;

/**
 * A named anchor (`<a id="account-credentials"></a>`) as an empty element with its id; any other
 * raw HTML, nothing. Inline in a paragraph, its opening tag is a node of its own (its `</a>`, the
 * next, is dropped).
 */
const namedAnchor: HtmlHandler = (_state, node: { value: string }) => {
  const id = /^<a id="([A-Za-z][\w-]*)">(?:<\/a>)?$/.exec(node.value.trim())?.[1];
  return id === undefined ? undefined : element("span", { id }, []);
};

async function toHtml(source: string, plugins: PluggableList): Promise<string> {
  return String(await unified().use(remarkParse).use(remarkGfm).use(remarkRehype, { handlers: { html: namedAnchor } }).use(plugins).use(rehypeStringify).process(source));
}

/** Renders a markdown file of the repository, given by its path from the repository root. */
export async function renderDoc(file: string): Promise<RenderedDoc> {
  const source = await readFile(resolve(REPO_ROOT, file), "utf8");
  const outline = { title: "", description: "", toc: [] as TocEntry[] };
  const html = await toHtml(source, [
    rehypeSlug, [rehypeOutline, outline], [rehypeAutolinkHeadings, anchors], [rehypeLinks, file], [rehypeCodeBlocks, file], rehypeTables,
  ]);
  return { title: outline.title, description: summarize(outline.description), html, toc: outline.toc };
}

/** A short description (an API field's, an operation's), its links rewritten as `file`'s are. */
export async function renderFragment(source: string, file: string): Promise<string> {
  return toHtml(source, [[rehypeLinks, file], [rehypeCodeBlocks, file], rehypeTables]);
}

/** `Request ids` → `Request-ids`, as Redoc wrote its section anchors. */
export const sectionSlug = (title: string) => title.trim().replace(/\s+/g, "-");

/**
 * A section of a longer text (the API reference's introduction), its headings' ids under `prefix`
 * as Redoc wrote them (`section/Errors/deposit_not_final`), so links to the old reference resolve.
 */
export async function renderSection(source: string, file: string, prefix: string): Promise<{ html: string; children: { id: string; title: string }[] }> {
  const children: { id: string; title: string }[] = [];
  // The section is one level under the page's H2, so its headings go one level down too.
  const ids = () => (tree: Root) => {
    visit(tree, "element", (node: Element) => {
      if (!/^h[1-4]$/.test(node.tagName)) return;
      node.tagName = `h${Number(node.tagName.slice(1)) + 1}`;
      const title = toString(node).trim();
      node.properties.id = `${prefix}/${sectionSlug(title)}`;
      children.push({ id: node.properties.id, title });
    });
  };
  const html = await toHtml(source, [ids, [rehypeAutolinkHeadings, { ...anchors, test: ["h3", "h4", "h5"] }], [rehypeLinks, file], [rehypeCodeBlocks, file], rehypeTables]);
  return { html, children };
}
