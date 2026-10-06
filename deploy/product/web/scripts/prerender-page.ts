const ROOT_MARKER = '<div id="root"></div>';
export const HEAD_MARKER = "<!--app-head-->";

/** Vite SSR's exact placeholders preserve the client entry and generated asset links. */
export function renderPage(template: string, rendered: { head: string; html: string }): string {
  if (!template.includes(ROOT_MARKER)) throw new Error("Missing root marker in HTML template");
  if (!template.includes(HEAD_MARKER)) throw new Error("Missing head marker in HTML template");
  if (/\sstyle\s*=/i.test(rendered.html)) throw new Error("Inline style violates the CSP");
  return template.replace(HEAD_MARKER, () => rendered.head)
    .replace(ROOT_MARKER, () => `<div id="root">${rendered.html}</div>`);
}
