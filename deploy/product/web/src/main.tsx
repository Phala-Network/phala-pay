import "@fontsource-variable/geist";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { mountChrome } from "./chrome.js";
import { DemoIsland } from "./DemoIsland.js";
import "./index.css";

mountChrome();
const demo = document.getElementById("demo-root");
if (demo !== null) createRoot(demo).render(<StrictMode><DemoIsland /></StrictMode>);
