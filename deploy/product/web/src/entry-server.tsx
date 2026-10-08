import type { ReactNode } from "react";
import { renderToString } from "react-dom/server";
import { Header } from "./Header.js";
import { ISLAND_PREFIXES } from "./islands.js";
import { renderHead } from "./Head.js";
import { pageMetadata, structuredData, type Page } from "./content/head.js";
import { ComparePage } from "./ComparePage.js";
import { NotFoundPage } from "./NotFoundPage.js";
import { ClosingCta, CompareTeaser, DemoSection, DeployCommand, Faq, Hero, HeroCode, SiteFooter, WhereTheMoneyGoes } from "./Site.js";

/**
 * An island: rendered on its own, with the identifier prefix its client root hydrates with, so the
 * ids React generates inside it match.
 */
function Island({ id, prefix, children, className }: { id: string; prefix: string; children: ReactNode; className?: string }) {
  return <div id={id} className={className} dangerouslySetInnerHTML={{ __html: renderToString(children, { identifierPrefix: prefix }) }} />;
}

function Main({ page }: { page: Page }) {
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
  }
}

/** Render only static content: App, browser theme state and demo queries stay client-side. */
export function render(page: Page): { head: string; html: string } {
  const html = renderToString(
    <div className="flex min-h-svh flex-col">
      <Island id="site-header" prefix={ISLAND_PREFIXES.header} className="sticky top-0 z-50"><Header /></Island>
      <Main page={page} />
      <Island id="site-footer" prefix={ISLAND_PREFIXES.footer}><SiteFooter /></Island>
    </div>,
  );
  return { head: renderHead(pageMetadata(page), structuredData(page)), html };
}
