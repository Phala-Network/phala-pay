import { HEAD_MARKER } from "../src/content/template.ts";

/** Vite SSR's exact placeholders preserve the client entry and generated asset links. */
export function renderPage(template: string, rendered: { head: string; html: string }, rootMarker: string): string {
  if (!template.includes(rootMarker)) throw new Error("Missing root marker in HTML template");
  if (!template.includes(HEAD_MARKER)) throw new Error("Missing head marker in HTML template");
  return template.replace(HEAD_MARKER, () => rendered.head)
    .replace(rootMarker, () => `<div id="root">${rendered.html}</div>`);
}
