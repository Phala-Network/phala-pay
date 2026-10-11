// The head's one inline script, before anything is painted: the visitor's theme; and, on a link to
// a part of the page (/#demo, a docs section), the page opened at that part from its first frame.
// Left to itself the browser paints as it parses (a slow device shows the page's top before the part
// exists) and scrolls to the part only once the page's deferred scripts have run. So on a fresh
// navigation with a fragment, rendering waits until the page's last element, its footer island, is
// parsed (`rel=expect`, render-blocking as the HTML standard defines it; ignored where unsupported),
// and the part is scrolled into view as that element arrives, before the first frame, the whole
// page above and around it laid out. A reload or a move through history keeps the browser's own
// restored position. Exact bootstrap bytes are authorized by the CSP hash, checked during prerender.
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
    let id;
    try {
      id = decodeURIComponent(location.hash.slice(1));
    } catch {
      return;
    }
    const last = "site-footer";
    const expect = document.createElement("link");
    expect.rel = "expect";
    expect.href = "#" + last;
    expect.setAttribute("blocking", "render");
    document.head.append(expect);
    const parsed = new MutationObserver(() => {
      if (document.getElementById(last) === null) return;
      parsed.disconnect();
      document.getElementById(id)?.scrollIntoView();
    });
    parsed.observe(document.documentElement, { childList: true, subtree: true });
  }
})();`;
