import sdkPackage from "../../../../../sdk/js/package.json" with { type: "json" };
import { COMPARE_ACCESSED, COMPARE_DESCRIPTION, COMPARE_KEYWORDS, COMPARE_TITLE } from "./compare.ts";
import { FAQ, HOME_DESCRIPTION, HOME_KEYWORDS, HOME_TITLE, NOT_FOUND_TITLE } from "./site.ts";

import { PAGES, type Page } from "./template.ts";
export type { Page } from "./template.ts";
/** A page's head: its canonical URL, or none for a page kept out of search results (the 404 page). */
export interface PageMetadata { title: string; description: string; keywords: string | null; url: string | null }
const origin = "https://pay.phala.com";
const repo = "https://github.com/Phala-Network/phala-pay";

export function pageMetadata(page: Page): PageMetadata {
  switch (page) {
    case "home":
      return { title: HOME_TITLE, description: HOME_DESCRIPTION, keywords: HOME_KEYWORDS, url: `${origin}${PAGES.home.path}` };
    case "compare":
      return { title: COMPARE_TITLE, description: COMPARE_DESCRIPTION, keywords: COMPARE_KEYWORDS, url: `${origin}${PAGES.compare.path}` };
    case "notFound":
      return { title: NOT_FOUND_TITLE, description: HOME_DESCRIPTION, keywords: null, url: null };
  }
}

export function structuredData(page: Page): Record<string, unknown> {
  const metadata = pageMetadata(page);
  const home = page === "home";
  const organization = {
    "@type": "Organization", "@id": `${origin}/#organization`, name: "Phala Network", url: "https://phala.com",
    logo: "https://phala.com/home/logo.svg", sameAs: ["https://github.com/Phala-Network"],
  };
  const website = {
    "@type": "WebSite", "@id": `${origin}/#website`, name: "Phala Pay", url: `${origin}/`,
    description: HOME_DESCRIPTION, publisher: { "@id": `${origin}/#organization` },
  };
  if (page === "notFound") return { "@context": "https://schema.org", "@graph": [organization, website] };
  return { "@context": "https://schema.org", "@graph": home ? [
      organization, website,
      {
        "@type": "SoftwareApplication", name: "Phala Pay", url: `${origin}/`, applicationCategory: "FinanceApplication",
        softwareVersion: sdkPackage.version, license: `${repo}/blob/main/LICENSE`, offers: { "@type": "Offer", price: "0", priceCurrency: "USD" },
        publisher: { "@id": `${origin}/#organization` }, description: HOME_DESCRIPTION,
      },
      { "@type": "SoftwareSourceCode", name: "Phala Pay", codeRepository: repo, license: `${repo}/blob/main/LICENSE` },
      { "@type": "FAQPage", mainEntity: FAQ.map(({ question, answer }) => ({ "@type": "Question", name: question, acceptedAnswer: { "@type": "Answer", text: answer } })) },
    ] : [
      organization, website,
      { "@type": "WebPage", "@id": `${origin}/compare#webpage`, url: metadata.url, name: metadata.title, description: metadata.description, dateModified: COMPARE_ACCESSED, isPartOf: { "@id": `${origin}/#website` }, breadcrumb: { "@id": `${origin}/compare#breadcrumb` } },
      { "@type": "BreadcrumbList", "@id": `${origin}/compare#breadcrumb`, itemListElement: [
        { "@type": "ListItem", position: 1, name: "Phala Pay", item: `${origin}/` },
        { "@type": "ListItem", position: 2, name: "Compare", item: `${origin}/compare` },
      ] },
    ] };
}
