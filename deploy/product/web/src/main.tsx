import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
// The SDK's components (the demo's checkout) in the page's stylesheet, before the site's own, which
// themes them (src/index.css): loaded with the page, not later with the demo's code.
import "@phala/pay-react/styles.css";
import { lazy, StrictMode } from "react";
import { hydrateRoot } from "react-dom/client";
import { mountChrome } from "./chrome.js";
import { DemoIsland } from "./DemoIsland.js";
import { ISLAND_PREFIXES } from "./islands.js";
import { DeployCommand, HeroCode } from "./Site.js";
import "./index.css";

mountChrome();
const heroCode = document.getElementById("hero-code");
if (heroCode !== null) hydrateRoot(heroCode, <StrictMode><HeroCode /></StrictMode>, { identifierPrefix: ISLAND_PREFIXES.heroCode });
const deployCommand = document.getElementById("deploy-command");
if (deployCommand !== null) hydrateRoot(deployCommand, <StrictMode><DeployCommand /></StrictMode>, { identifierPrefix: ISLAND_PREFIXES.deployCommand });
// The demo's code (its wallet and chain libraries) loads on its own, after the page's.
const Demo = lazy(() => import("./Demo.js"));
const demo = document.getElementById("demo-root");
if (demo !== null) hydrateRoot(demo, <StrictMode><DemoIsland demo={Demo} /></StrictMode>, { identifierPrefix: ISLAND_PREFIXES.demo });
