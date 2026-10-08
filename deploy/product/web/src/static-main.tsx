import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import { mountChrome } from "./chrome.js";
import "./index.css";

// A page without islands of its own (the comparison, the 404 page): its body stays prerendered, and
// only the shared chrome is mounted on the client.
mountChrome();
