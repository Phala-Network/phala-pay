import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import { mountChrome } from "./chrome.js";
import "./index.css";

// The docs and the API reference stay prerendered. On the client the shared chrome is mounted, and
// each code block's copy button, hidden in the static HTML where it could not work, is wired up.
mountChrome();
for (const button of document.querySelectorAll<HTMLButtonElement>("button[data-copy]")) {
  const code = button.closest(".code-block")?.querySelector("pre");
  if (code == null) continue;
  button.hidden = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  button.addEventListener("click", () => {
    navigator.clipboard.writeText(code.textContent).then(
      () => { button.textContent = "Copied"; },
      () => { button.textContent = "Could not copy"; },
    ).finally(() => {
      clearTimeout(timer);
      timer = setTimeout(() => { button.textContent = "Copy"; }, 1500);
    });
  });
}
