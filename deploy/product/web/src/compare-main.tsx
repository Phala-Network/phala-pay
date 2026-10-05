import "@fontsource-variable/geist";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { SiteFooter, SiteHeader } from "./Site.js";
import { useTheme } from "./theme.js";
import "./index.css";

function Header() {
  const [theme, setTheme] = useTheme();
  return <SiteHeader theme={theme} onThemeChange={setTheme} />;
}

// The comparison body stays prerendered. Only the shared chrome is mounted on the client.
const header = document.getElementById("site-header");
const footer = document.getElementById("site-footer");
if (header !== null) createRoot(header).render(<StrictMode><Header /></StrictMode>);
if (footer !== null) createRoot(footer).render(<StrictMode><SiteFooter /></StrictMode>);
