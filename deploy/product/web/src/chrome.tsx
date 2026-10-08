import { StrictMode } from "react";
import { hydrateRoot } from "react-dom/client";
import { isSection, SiteFooter } from "./Site.js";
import { Header } from "./Header.js";
import { ISLAND_PREFIXES } from "./islands.js";

/** Mount shared interactions without replacing the static page body. */
export function mountChrome() {
  const header = document.getElementById("site-header");
  const footer = document.getElementById("site-footer");
  if (header !== null) {
    const current = header.dataset["current"];
    hydrateRoot(header, <StrictMode><Header current={isSection(current) ? current : null} /></StrictMode>, { identifierPrefix: ISLAND_PREFIXES.header });
  }
  if (footer !== null) hydrateRoot(footer, <StrictMode><SiteFooter /></StrictMode>, { identifierPrefix: ISLAND_PREFIXES.footer });
}
