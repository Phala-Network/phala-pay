import { ArrowLeft, ArrowRight, ChevronDown } from "lucide-react";
import type { ReactNode } from "react";
import { cn } from "@/lib/utils";
import { DOC_SECTIONS, DOCS, docPath, type DocEntry } from "./content/docs.js";
import type { RenderedDoc, TocEntry } from "../scripts/markdown.ts";
import { GRID } from "./layout.js";
import { CONTAINER, Eyebrow, REPO } from "./Site.js";

/**
 * The docs' and the reference's layout, on the page's grid (src/layout.ts): the navigation in
 * columns 1–3, the one part of the site that may stick and scroll on its own; the page in
 * columns 4–12, its text at a reading measure. Both start their first line on one baseline: the
 * navigation's first label and the page's kicker share a style (LABEL).
 */
export const SIDEBAR_LAYOUT = cn(CONTAINER, GRID, "flex-1");
export const NAV_COLUMN = "hidden lg:col-span-3 lg:block";
export const NAV_SCROLL = "sticky top-16 max-h-[calc(100svh-4rem)] overflow-y-auto py-10 pr-2";
export const PAGE_COLUMN = "min-w-0 pt-10 pb-24 lg:col-span-9 lg:col-start-4";
/** A small label (src/index.css, `mono-label`), muted: the one kind of text under 14px. */
export const LABEL = "mono-label text-muted-foreground";
/**
 * A link in a sidebar: the current page marked by the brand's rule at its edge, on the muted fill.
 */
export const NAV_LINK = "flex min-h-11 items-center rounded-r-md border-l-2 border-transparent px-2 py-1 text-body-foreground transition-colors hover:bg-muted hover:text-foreground aria-[current=page]:border-brand-ink aria-[current=page]:bg-muted aria-[current=page]:font-medium aria-[current=page]:text-foreground lg:min-h-8";

/**
 * Rendered markdown, in the site's type: the `docs-prose` rules in src/index.css set its headings,
 * lists, tables, and code blocks on the design system's scale and colours.
 */
export const PROSE = "docs-prose";

/** A doc as the build renders it (scripts/content-plugin.ts): its slug, its file, and its content. */
export interface DocContent extends RenderedDoc { slug: string; file: string }

/**
 * The docs' navigation: each section's pages, the current one marked, with its sections under it
 * (its H2s), so the page's outline is in the one column that may scroll on its own.
 */
function DocsNav({ current, toc = [] }: { current: string | null; toc?: TocEntry[] }) {
  return (
    <nav aria-label="Documentation" className="text-sm">
      {DOC_SECTIONS.map(({ title, pages }) => (
        <div key={title} className="mb-6 last:mb-0">
          <p className={cn(LABEL, "pr-2 pb-1.5 pl-2.5")}>{title}</p>
          <ul>
            {pages.map(({ slug, label }) => (
              <li key={slug}>
                <a href={docPath(slug)} aria-current={slug === current ? "page" : undefined} className={NAV_LINK}>
                  {label}
                </a>
                {slug === current && toc.some(({ depth }) => depth === 2) && (
                  <ul aria-label="On this page" className="my-1 ml-2 border-l xl:hidden">
                    {toc.filter(({ depth }) => depth === 2).map(({ id, text }) => (
                      <li key={id}>
                        <a href={`#${id}`} className="-ml-px flex min-h-11 items-center border-l border-transparent py-1 pr-2 pl-3 text-muted-foreground transition-colors hover:border-foreground hover:text-foreground lg:min-h-7">
                          {text}
                        </a>
                      </li>
                    ))}
                  </ul>
                )}
              </li>
            ))}
          </ul>
        </div>
      ))}
      <div className="border-t pt-4">
        <a href="/reference" aria-current={current === null ? "page" : undefined} className={cn(NAV_LINK, "font-medium")}>
          API reference
        </a>
      </div>
    </nav>
  );
}

/**
 * Below lg, a page's navigation folds into one row at its top: native, so it works without script.
 * Opened, it pushes the page down; nothing scrolls inside it.
 */
export function DocsMobileNav({ label, children }: { label: string; children: ReactNode }) {
  return (
    <details className="group mb-8 rounded-xl border bg-card shadow-card lg:hidden">
      <summary className="flex min-h-12 cursor-pointer list-none items-center justify-between px-4 text-sm font-medium [&::-webkit-details-marker]:hidden">
        {label}
        <ChevronDown aria-hidden="true" strokeWidth={1.75} className="size-4 text-muted-foreground group-open:rotate-180 motion-safe:transition-transform" />
      </summary>
      <div className="border-t px-2 py-3">{children}</div>
    </details>
  );
}

function Neighbour({ entry, direction }: { entry: DocEntry | undefined; direction: "previous" | "next" }) {
  if (entry === undefined) return <span />;
  return (
    <a href={docPath(entry.slug)} rel={direction === "previous" ? "prev" : "next"}
      className={cn("group flex flex-col gap-1 rounded-xl border bg-card px-5 py-4 shadow-card transition-colors hover:border-border-strong", direction === "next" && "items-end text-right")}>
      <span className={cn(LABEL, "flex items-center gap-1.5")}>
        {direction === "previous" && <ArrowLeft aria-hidden="true" className="size-3.5" />}
        {direction === "previous" ? "Previous" : "Next"}
        {direction === "next" && <ArrowRight aria-hidden="true" className="size-3.5" />}
      </span>
      <span className="font-medium">{entry.label}</span>
    </a>
  );
}

/**
 * From xl, the page's sections beside its text, staying in view; below xl they are under the page's
 * entry in the navigation. A long outline scrolls on its own, as the navigation does.
 */
function Outline({ toc }: { toc: TocEntry[] }) {
  const sections = toc.filter(({ depth }) => depth === 2);
  if (sections.length === 0) return null;
  return (
    <aside className="hidden xl:col-span-2 xl:block">
      <div className="sticky top-16 max-h-[calc(100svh-4rem)] overflow-y-auto pb-10">
        <nav aria-label="On this page" className="text-sm">
          <p className={cn(LABEL, "pb-2")}>On this page</p>
          <ul className="border-l">
            {sections.map(({ id, text }) => (
              <li key={id}>
                <a href={`#${id}`} className="-ml-px flex min-h-7 items-center border-l border-transparent py-1 pl-3 text-muted-foreground transition-colors hover:border-foreground hover:text-foreground">
                  {text}
                </a>
              </li>
            ))}
          </ul>
        </nav>
      </div>
    </aside>
  );
}

export function DocsPage({ doc }: { doc: DocContent }) {
  const index = DOCS.findIndex(({ slug }) => slug === doc.slug);
  const section = DOC_SECTIONS.find(({ pages }) => pages.some(({ slug }) => slug === doc.slug));
  return (
    <div data-layout="sidebar" className={SIDEBAR_LAYOUT}>
      {/* The one part of the site that scrolls on its own: the docs' navigation, beside the page. */}
      <aside data-column="left" className={NAV_COLUMN}>
        <div className={NAV_SCROLL}>
          <DocsNav current={doc.slug} toc={doc.toc} />
        </div>
      </aside>
      <main id="top" data-column="right" className={PAGE_COLUMN}>
        <DocsMobileNav label="Documentation menu"><DocsNav current={doc.slug} toc={doc.toc} /></DocsMobileNav>
        {/* From xl, the page's nine columns again: the text in seven, its outline in two beside it. */}
        <div className="xl:grid xl:grid-cols-9 xl:gap-x-8">
          <article className="max-w-3xl min-w-0 xl:col-span-7">
            {section !== undefined && <Eyebrow>{section.title}</Eyebrow>}
            <h1 className="mt-4 text-title-sm font-semibold text-balance sm:text-title">{doc.title}</h1>
            <div className={cn(PROSE, "mt-8")} dangerouslySetInnerHTML={{ __html: doc.html }} />
            <footer className="mt-16 border-t pt-8">
              <div className="grid gap-3 sm:grid-cols-2">
                <Neighbour entry={DOCS[index - 1]} direction="previous" />
                <Neighbour entry={DOCS[index + 1]} direction="next" />
              </div>
              <p className="mt-8 text-sm text-muted-foreground">
                This page is <a className="font-medium text-foreground underline decoration-foreground/30 underline-offset-4 hover:decoration-foreground" href={`${REPO}/blob/main/${doc.file}`}>{doc.file}</a> in the repository, rendered at build time.
              </p>
            </footer>
          </article>
          <Outline toc={doc.toc} />
        </div>
      </main>
    </div>
  );
}
