// Synchronous external script: the CSP permits no inline scripts.
(() => {
  let stored;
  try {
    stored = localStorage.getItem("demo-theme");
  } catch {
    // Browser privacy settings may disable storage; use the system preference.
  }
  const dark = stored === "dark" || (stored !== "light" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
})();
