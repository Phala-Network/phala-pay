import { ArrowLeft, ArrowRight, ChevronDown } from "lucide-react";
import type { ReactNode } from "react";
import { cn } from "@/lib/utils";
import { DOC_SECTIONS, DOCS, docPath, type DocEntry } from "./content/docs.js";
import type { RenderedDoc, TocEntry } from "../scripts/markdown.ts";
import { CONTAINER, REPO } from "./Site.js";

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
          <p className="px-2 pb-1.5 text-xs font-medium text-muted-foreground">{title}</p>
          <ul>
            {pages.map(({ slug, label }) => (
              <li key={slug}>
                <a href={docPath(slug)} aria-current={slug === current ? "page" : undefined}
                  className="flex min-h-11 items-center rounded-md px-2 py-1 text-body-foreground transition-colors hover:bg-muted hover:text-foreground aria-[current=page]:bg-muted aria-[current=page]:font-medium aria-[current=page]:text-foreground lg:min-h-8">
                  {label}
                </a>
                {slug === current && toc.some(({ depth }) => depth === 2) && (
                  <ul aria-label="On this page" className="my-1 ml-2 border-l">
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
        <a href="/reference" aria-current={current === null ? "page" : undefined}
          className="flex min-h-11 items-center rounded-md px-2 font-medium text-body-foreground transition-colors hover:bg-muted hover:text-foreground aria-[current=page]:bg-muted aria-[current=page]:text-foreground lg:min-h-8">
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
    <details className="group mb-8 rounded-lg border lg:hidden">
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
      className={cn("group flex flex-col gap-1 rounded-lg border px-4 py-3 transition-colors hover:border-border-strong hover:bg-muted/50", direction === "next" && "items-end text-right")}>
      <span className="flex items-center gap-1.5 text-xs text-muted-foreground">
        {direction === "previous" && <ArrowLeft aria-hidden="true" className="size-3.5" />}
        {direction === "previous" ? "Previous" : "Next"}
        {direction === "next" && <ArrowRight aria-hidden="true" className="size-3.5" />}
      </span>
      <span className="font-medium">{entry.label}</span>
    </a>
  );
}

export function DocsPage({ doc }: { doc: DocContent }) {
  const index = DOCS.findIndex(({ slug }) => slug === doc.slug);
  const section = DOC_SECTIONS.find(({ pages }) => pages.some(({ slug }) => slug === doc.slug));
  return (
    <div className={`${CONTAINER} flex-1 lg:grid lg:grid-cols-[15rem_minmax(0,1fr)] lg:gap-10 xl:gap-16`}>
      {/* The one part of the site that scrolls on its own: the docs' navigation, beside the page. */}
      <aside className="hidden lg:block">
        <div className="sticky top-16 max-h-[calc(100svh-4rem)] overflow-y-auto py-10 pr-2">
          <DocsNav current={doc.slug} toc={doc.toc} />
        </div>
      </aside>
      <main id="top" className="min-w-0 pt-10 pb-24">
        <DocsMobileNav label="Documentation menu"><DocsNav current={doc.slug} toc={doc.toc} /></DocsMobileNav>
        <article className="mx-auto max-w-3xl">
          {section !== undefined && <p className="text-sm font-medium text-muted-foreground">{section.title}</p>}
          <h1 className="mt-2 text-title-sm font-semibold text-balance sm:text-title">{doc.title}</h1>
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
      </main>
    </div>
  );
}
