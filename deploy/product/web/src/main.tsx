import "@fontsource-variable/geist";
import { StrictMode } from "react";
import { hydrateRoot } from "react-dom/client";
import { mountChrome } from "./chrome.js";
import { DemoIsland } from "./DemoIsland.js";
import { ISLAND_PREFIXES } from "./islands.js";
import "./index.css";

mountChrome();
const demo = document.getElementById("demo-root");
if (demo !== null) hydrateRoot(demo, <StrictMode><DemoIsland /></StrictMode>, { identifierPrefix: ISLAND_PREFIXES.demo });
