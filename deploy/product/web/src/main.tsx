import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import { StrictMode } from "react";
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
const demo = document.getElementById("demo-root");
if (demo !== null) hydrateRoot(demo, <StrictMode><DemoIsland /></StrictMode>, { identifierPrefix: ISLAND_PREFIXES.demo });
