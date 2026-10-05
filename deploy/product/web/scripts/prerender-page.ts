export interface PageMetadata { title: string; description: string; url: string }
const ROOT_MARKER = '<div id="root"></div>';

function escapeAttribute(value: string): string {
  return value.replaceAll("&", "&amp;").replaceAll('"', "&quot;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

/** Preserve Vite's entry assets while replacing content and search metadata. */
export function renderPage(template: string, content: string, metadata: PageMetadata, graph: Record<string, unknown>): string {
  if (!template.includes(ROOT_MARKER)) throw new Error("Missing root marker in HTML template");
  if (/\sstyle\s*=/i.test(content)) throw new Error("Inline style violates the CSP");
  const title = escapeAttribute(metadata.title);
  const description = escapeAttribute(metadata.description);
  const url = escapeAttribute(metadata.url);
  let html = template.replace(ROOT_MARKER, () => `<div id="root">${content}</div>`)
    .replace(/<title>[^<]*<\/title>/, () => `<title>${title}</title>`)
    .replace(/(<meta\s+(?:name|property)="(?:description|og:description|twitter:description)"\s+content=")[^"]*("\s*\/>)/g, (_match, prefix: string, suffix: string) => `${prefix}${description}${suffix}`)
    .replace(/(<meta\s+(?:name|property)="(?:og:title|twitter:title)"\s+content=")[^"]*("\s*\/>)/g, (_match, prefix: string, suffix: string) => `${prefix}${title}${suffix}`)
    .replace(/(<link\s+rel="canonical"\s+href=")[^"]*("\s*\/>)/, (_match, prefix: string, suffix: string) => `${prefix}${url}${suffix}`)
    .replace(/(<meta\s+property="og:url"\s+content=")[^"]*("\s*\/>)/, (_match, prefix: string, suffix: string) => `${prefix}${url}${suffix}`);
  const json = JSON.stringify(graph).replaceAll("<", "\\u003c");
  html = html.replace("</head>", () => `<script type="application/ld+json">${json}</script>\n</head>`);
  return html;
}
