import "@fontsource-variable/geist";
import { mountChrome } from "./chrome.js";
import "./index.css";

// The comparison body stays prerendered. Only the shared chrome is mounted on the client.
mountChrome();
