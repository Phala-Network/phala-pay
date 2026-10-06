import { renderToStaticMarkup } from "react-dom/server";
import type { PageMetadata } from "./content/head.js";
import { TAGLINE } from "./content/site.js";
import { THEME_SCRIPT } from "./content/theme-script.js";

/** React escapes metadata attributes and text; JSON-LD escapes script delimiters separately. */
export function renderHead(metadata: PageMetadata, graph: Record<string, unknown>): string {
  const image = "https://pay.phala.com/og-image.png";
  const alt = `Phala Pay: ${TAGLINE}`;
  return renderToStaticMarkup(<>
    <script dangerouslySetInnerHTML={{ __html: THEME_SCRIPT }} />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>{metadata.title}</title>
    <meta name="description" content={metadata.description} />
    {metadata.keywords !== null && <meta name="keywords" content={metadata.keywords} />}
    {metadata.url === null ? <meta name="robots" content="noindex" /> : <link rel="canonical" href={metadata.url} />}
    <meta name="color-scheme" content="light dark" />
    <meta name="theme-color" media="(prefers-color-scheme: light)" content="#fafafa" />
    <meta name="theme-color" media="(prefers-color-scheme: dark)" content="#0a0a0a" />
    <link rel="icon" href="/favicon.svg" type="image/svg+xml" />
    <link rel="icon" href="/favicon-32.png" type="image/png" sizes="32x32" />
    <link rel="apple-touch-icon" href="/apple-touch-icon.png" />
    <link rel="manifest" href="/site.webmanifest" />
    <meta property="og:type" content="website" />
    <meta property="og:site_name" content="Phala Pay" />
    <meta property="og:title" content={metadata.title} />
    <meta property="og:description" content={metadata.description} />
    {metadata.url !== null && <meta property="og:url" content={metadata.url} />}
    <meta property="og:image" content={image} />
    <meta property="og:image:type" content="image/png" />
    <meta property="og:image:width" content="1200" />
    <meta property="og:image:height" content="630" />
    <meta property="og:image:alt" content={alt} />
    <meta name="twitter:title" content={metadata.title} />
    <meta name="twitter:description" content={metadata.description} />
    <meta name="twitter:card" content="summary_large_image" />
    <meta name="twitter:image" content={image} />
    <meta name="twitter:image:alt" content={alt} />
    <script type="application/ld+json" dangerouslySetInnerHTML={{ __html: JSON.stringify(graph).replaceAll("<", "\\u003c") }} />
  </>);
}
