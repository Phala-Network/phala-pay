// Exact bootstrap bytes are authorized by the CSP hash, checked during prerender.
export const THEME_SCRIPT = `(() => {
  let stored;
  try {
    stored = localStorage.getItem("demo-theme");
  } catch {
    // Browser privacy settings may disable storage; use the system preference.
  }
  const dark = stored === "dark" || (stored !== "light" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
})();`;
