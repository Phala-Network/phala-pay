import { renderToString } from "react-dom/server";
import { Header } from "./Header.js";
import { ISLAND_PREFIXES } from "./islands.js";
import { renderHead } from "./Head.js";
import { pageMetadata, structuredData, type Page } from "./content/head.js";
import { ComparePage } from "./ComparePage.js";
import { ClosingCta, CompareTeaser, DemoSection, Faq, Hero, HowItWorks, Properties, SiteFooter } from "./Site.js";

/** Render only static content: App, browser theme state and demo queries stay client-side. */
export function render(page: Page): { head: string; html: string } {
  const header = renderToString(<Header />, { identifierPrefix: ISLAND_PREFIXES.header });
  const footer = renderToString(<SiteFooter />, { identifierPrefix: ISLAND_PREFIXES.footer });
  const html = renderToString(
    <div className="flex min-h-svh flex-col">
      <div id="site-header" className="sticky top-0 z-50" dangerouslySetInnerHTML={{ __html: header }} />
      {page === "home" ? (
        <main id="top" className="flex-1">
          <Hero />
          <HowItWorks />
          <DemoSection />
          <Properties />
          <CompareTeaser />
          <Faq />
          <ClosingCta />
        </main>
      ) : <ComparePage />}
      <div id="site-footer" dangerouslySetInnerHTML={{ __html: footer }} />
    </div>,
  );
  return { head: renderHead(pageMetadata(page), structuredData(page)), html };
}
