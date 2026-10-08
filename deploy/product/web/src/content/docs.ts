// The documentation the site renders at /docs, from the repository's own markdown (the source of
// truth; nothing is copied): each page's file, its path on the site, its place in the docs'
// navigation, and its meta description. Only the pages written for merchants and operators are
// here (docs/README.md groups them by reader); the specification, design notes, and plans stay
// on GitHub, where the rendered pages link.
export interface DocEntry { slug: string; file: string; label: string; description: string }
export interface DocSection { title: string; pages: DocEntry[] }

export const DOC_SECTIONS: DocSection[] = [
  {
    title: "Get started",
    pages: [
      { slug: "", file: "docs/README.md", label: "Documentation",
        description: "Phala Pay's documentation for merchants who integrate and operators who self-host: guides, SDKs, configuration, and the API reference." },
      { slug: "overview", file: "docs/overview.md", label: "How Phala Pay works",
        description: "Phala Pay's model: per-payment deposit addresses that pay only your treasury, the payment lifecycle, and who owns what." },
    ],
  },
  {
    title: "Integrate",
    pages: [
      { slug: "integration", file: "docs/integration.md", label: "Integration guide",
        description: "Integrate Phala Pay: quotes, deposit addresses, treasuries, sweeps, signed webhooks and fulfillment, refunds, testing, and go-live." },
      { slug: "sdk/js", file: "sdk/js/README.md", label: "@phala/pay",
        description: "@phala/pay, the framework-free browser checkout core: render a quote's checkout or a deposit address, and follow its payments." },
      { slug: "sdk/react", file: "sdk/js-react/README.md", label: "@phala/pay-react",
        description: "@phala/pay-react: React checkout and deposit-address components, hooks, icons, and styles for Phala Pay." },
      { slug: "sdk/server", file: "sdk/js-server/README.md", label: "@phala/pay-server",
        description: "@phala/pay-server: the merchant server client for Phala Pay's API, with its types, webhook verification, and offline helpers." },
      { slug: "sdk/python", file: "sdk/python/README.md", label: "phala-pay (Python)",
        description: "phala-pay, the Python client for Phala Pay's API: quotes, deposit addresses, refunds, sweeps, and webhook verification." },
      { slug: "sandbox", file: "deploy/sandbox/README.md", label: "Integrator sandbox",
        description: "Phala Pay's integrator sandbox: scripted late, under, over, rejected, and refused payments on testnet, locally or in test mode." },
    ],
  },
  {
    title: "Self-host",
    pages: [
      { slug: "self-hosting", file: "docs/self-hosting.md", label: "Self-hosting guide",
        description: "Self-host Phala Pay: the steps, in order, from an environment repository and a verified release to a credited test deposit and going live." },
      { slug: "deployment", file: "deploy/README.md", label: "Deployment reference",
        description: "Deploying and running a Phala Pay instance: releases, sealed secrets, attested settings, RPC providers, domains, monitoring, and onboarding." },
      { slug: "configuration", file: "docs/configuration.md", label: "Service configuration",
        description: "Phala Pay's service configuration: the topup commands, the configuration file, flags, and environment variables." },
    ],
  },
  {
    title: "Reference",
    pages: [
      { slug: "changelog", file: "CHANGELOG.md", label: "Changelog",
        description: "Phala Pay's releases: each version's changes to the HTTP API, webhooks, and SDKs." },
    ],
  },
];

export const DOCS: DocEntry[] = DOC_SECTIONS.flatMap(({ pages }) => pages);

/** A doc's path on the site: `/docs` for the index, else `/docs/<slug>`. */
export function docPath(slug: string): string {
  return slug === "" ? "/docs" : `/docs/${slug}`;
}
