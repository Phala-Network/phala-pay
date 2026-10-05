import { renderToString } from "react-dom/server";
import { ComparePage } from "./ComparePage.js";
import { ClosingCta, CompareTeaser, DemoSection, Faq, Hero, HowItWorks, Properties, SiteFooter, SiteHeader } from "./Site.js";

/** Render only static content: App, browser theme state and demo queries stay client-side. */
export function render(page: "home" | "compare"): string {
  return renderToString(
    <div className="flex min-h-svh flex-col">
      <div id="site-header"><SiteHeader theme="light" onThemeChange={() => undefined} /></div>
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
      <div id="site-footer"><SiteFooter /></div>
    </div>,
  );
}
