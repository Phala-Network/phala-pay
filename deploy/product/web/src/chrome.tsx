import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { SiteFooter, SiteHeader } from "./Site.js";
import { useTheme } from "./theme.js";

function Header() {
  const [theme, setTheme] = useTheme();
  return <SiteHeader theme={theme} onThemeChange={setTheme} />;
}

/** Mount shared interactions without replacing the static page body. */
export function mountChrome() {
  const header = document.getElementById("site-header");
  const footer = document.getElementById("site-footer");
  if (header !== null) createRoot(header).render(<StrictMode><Header /></StrictMode>);
  if (footer !== null) createRoot(footer).render(<StrictMode><SiteFooter /></StrictMode>);
}
