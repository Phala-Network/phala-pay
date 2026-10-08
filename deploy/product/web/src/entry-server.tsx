import type { ReactNode } from "react";
import { renderToString } from "react-dom/server";
import { Header } from "./Header.js";
import { ISLAND_PREFIXES } from "./islands.js";
import { renderHead } from "./Head.js";
import { pageMetadata, structuredData, type Page } from "./content/head.js";
import { ComparePage } from "./ComparePage.js";
import { NotFoundPage } from "./NotFoundPage.js";
import { DocsPage } from "./DocsPage.js";
import { ReferencePage } from "./ReferencePage.js";
import docs from "virtual:docs";
import reference from "virtual:reference";
import { ClosingCta, CompareTeaser, DemoSection, DeployCommand, Faq, Hero, HeroCode, SiteFooter, WhereTheMoneyGoes, type Section } from "./Site.js";

/**
 * An island: rendered on its own, with the identifier prefix its client root hydrates with, so the
 * ids React generates inside it match.
 */
function Island({ id, prefix, children, className, current }: { id: string; prefix: string; children: ReactNode; className?: string; current?: Section | null }) {
  return <div id={id} className={className} data-current={current ?? undefined} dangerouslySetInnerHTML={{ __html: renderToString(children, { identifierPrefix: prefix }) }} />;
}

/** The part of the site a page belongs to, for the header. */
function sectionOf(page: Page): Section | null {
  if (page.startsWith("doc:")) return "docs";
  return page === "compare" || page === "reference" ? page : null;
}

function Main({ page }: { page: Page }) {
  const doc = docOf(page);
  if (doc !== undefined) return <DocsPage doc={doc} />;
  switch (page) {
    case "home":
      return (
        <main id="top" className="flex-1">
          <Hero code={<Island id="hero-code" prefix={ISLAND_PREFIXES.heroCode}><HeroCode /></Island>} />
          <DemoSection />
          <WhereTheMoneyGoes />
          <CompareTeaser />
          <Faq />
          <ClosingCta command={<Island id="deploy-command" prefix={ISLAND_PREFIXES.deployCommand}><DeployCommand /></Island>} />
        </main>
      );
    case "compare":
      return <ComparePage />;
    case "notFound":
      return <NotFoundPage />;
    case "reference":
      return <ReferencePage model={reference} />;
    default:
      throw new Error(`No page ${page}`);
  }
}

/** A doc page's rendered markdown. */
function docOf(page: Page) {
  if (!page.startsWith("doc:")) return undefined;
  const doc = docs.find(({ slug }) => `doc:${slug}` === page);
  if (doc === undefined) throw new Error(`No rendered doc for ${page}`);
  return doc;
}

/** Render only static content: App, browser theme state and demo queries stay client-side. */
export function render(page: Page): { head: string; html: string } {
  const html = renderToString(
    <div className="flex min-h-svh flex-col">
      <Island id="site-header" prefix={ISLAND_PREFIXES.header} className="sticky top-0 z-50" current={sectionOf(page)}><Header current={sectionOf(page)} /></Island>
      <Main page={page} />
      <Island id="site-footer" prefix={ISLAND_PREFIXES.footer}><SiteFooter /></Island>
    </div>,
  );
  const doc = docOf(page);
  return { head: renderHead(pageMetadata(page, doc), structuredData(page, doc)), html };
}
