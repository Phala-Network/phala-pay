// The head's one inline script, before anything is painted: the visitor's theme; and, on a link to
// a part of the page (/#demo, a docs section), the page opened at that part from its first frame.
// The browser scrolls to it itself only once the page's deferred scripts have run, and paints the
// page's top before then; this scrolls as soon as the page is parsed, as the HTML standard's "scroll
// to the fragment" asks once the part exists (a reload or a move through history keeps the
// browser's own restored position). Exact bootstrap bytes are authorized by the CSP hash, checked
// during prerender.
export const THEME_SCRIPT = `(() => {
  let stored;
  try {
    stored = localStorage.getItem("demo-theme");
  } catch {
    // Browser privacy settings may disable storage; use the system preference.
  }
  const dark = stored === "dark" || (stored !== "light" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
  if (location.hash.length > 1 && performance.getEntriesByType("navigation")[0]?.type === "navigate") {
    document.addEventListener("readystatechange", () => {
      let id;
      try {
        id = decodeURIComponent(location.hash.slice(1));
      } catch {
        return;
      }
      document.getElementById(id)?.scrollIntoView();
    }, { once: true });
  }
})();`;
