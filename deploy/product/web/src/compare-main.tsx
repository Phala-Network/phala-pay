import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import { mountChrome } from "./chrome.js";
import "./index.css";

// The comparison body stays prerendered. Only the shared chrome is mounted on the client.
mountChrome();
