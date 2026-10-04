import { BookOpen, Braces, Cpu, Menu, Rocket, Server, Wallet, type LucideIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Sheet, SheetClose, SheetContent, SheetHeader, SheetTitle, SheetTrigger } from "@/components/ui/sheet";
import { GitHubIcon } from "./common.js";
import { ICON_BUTTON, ThemeToggle, type Theme } from "./theme.js";

const REPO = "https://github.com/Phala-Network/phala-pay";
const LINKS = {
  repo: REPO,
  docs: `${REPO}/blob/main/docs/integration.md`,
  selfHosting: `${REPO}/blob/main/docs/self-hosting.md`,
  // The guide's one-command deploy to your own Phala Cloud workspace, beside its other two paths.
  deploy: `${REPO}/blob/main/docs/self-hosting.md#one-command-deploy`,
  reference: "https://phala-network.github.io/phala-pay/",
  npm: "https://www.npmjs.com/package/@phala/pay-react",
  license: `${REPO}/blob/main/LICENSE`,
  security: `${REPO}/blob/main/SECURITY.md`,
};

/** The page's width. */
export const CONTAINER = "mx-auto w-full max-w-[84rem] px-5 sm:px-8 2xl:max-w-[92rem]";

// The headline, as index.html's title, description, and link preview (brand/og-image.svg) carry it.
const TAGLINE = "Fast, secure, non-custodial crypto payments";

// README.md; docs/self-hosting.md; docs/architecture.md §1; docs/integration.md §5.
const PROPERTIES: { icon: LucideIcon; title: string; text: string }[] = [
  {
    icon: Wallet,
    title: "Non-custodial",
    text: "Addresses can only pay your treasury; the service holds no funds and sends no transactions.",
  },
  {
    icon: Server,
    title: "Self-hosted",
    text: "Open source under Apache-2.0: you run your own instance, for your own merchants.",
  },
  {
    icon: Cpu,
    title: "Runs in a TEE",
    text: "A dstack confidential VM, with an attestation you can verify before you trust it.",
  },
  {
    icon: Braces,
    title: "Stripe-shaped API",
    text: "API keys, Idempotency-Key, metadata, Stripe's Event object, and Standard Webhooks.",
  },
];

const NAV = [
  { href: LINKS.docs, label: "Docs" },
  { href: LINKS.reference, label: "API reference" },
  { href: LINKS.selfHosting, label: "Self-hosting" },
];

export function SiteHeader({ theme, onThemeChange }: { theme: Theme; onThemeChange: (theme: Theme) => void }) {
  return (
    <header className="sticky top-0 z-50 border-b bg-background">
      <div className={`${CONTAINER} flex h-14 items-center justify-between gap-4`}>
        <a href="#top" className="flex rounded-md">
          <Lockup />
        </a>
        <nav aria-label="Site" className="-mr-3 flex items-center gap-1 text-muted-foreground">
          {NAV.map(({ href, label }) => (
            <Button key={label} variant="ghost" asChild className="hidden hover:text-foreground md:inline-flex">
              <a href={href}>{label}</a>
            </Button>
          ))}
          <a href={LINKS.repo} aria-label="GitHub" className={ICON_BUTTON}>
            <GitHubIcon />
          </a>
          <ThemeToggle theme={theme} onChange={onThemeChange} />
          <Sheet>
            <SheetTrigger asChild>
              <button type="button" className={`${ICON_BUTTON} md:hidden`} aria-label="Menu">
                <Menu aria-hidden="true" />
              </button>
            </SheetTrigger>
            <SheetContent side="right" className="w-72">
              <SheetHeader>
                <SheetTitle>Phala Pay</SheetTitle>
              </SheetHeader>
              <nav aria-label="Menu" className="flex flex-col gap-1 px-4">
                {NAV.map(({ href, label }) => (
                  <SheetClose asChild key={label}>
                    <a className="rounded-md px-2 py-2 text-sm font-medium hover:bg-accent" href={href}>
                      {label}
                    </a>
                  </SheetClose>
                ))}
              </nav>
            </SheetContent>
          </Sheet>
        </nav>
      </div>
    </header>
  );
}

/**
 * The logo, as brand/lockup-light.svg and, with the mark's edge on the dark theme,
 * brand/lockup-dark.svg: the mark and PHALA PAY in the lettering of Phala's logo, laid out in that
 * logo's units, where the mark is 48 units and the caps 16 (brand/README.md). At 32 px, the edge's
 * 1-unit ring is 1 px.
 */
function Lockup() {
  return (
    <svg viewBox="0 0 192 48" role="img" aria-label="Phala Pay" className="h-8 w-32 shrink-0">
      <g transform="scale(1.5)">
        <rect width="32" height="32" rx="8" className="fill-neutral-950" />
        <path
          fillRule="evenodd"
          d="M8 0H24A8 8 0 0 1 32 8V24A8 8 0 0 1 24 32H8A8 8 0 0 1 0 24V8A8 8 0 0 1 8 0ZM8 1A7 7 0 0 0 1 8V24A7 7 0 0 0 8 31H24A7 7 0 0 0 31 24V8A7 7 0 0 0 24 1Z"
          className="hidden fill-white/15 dark:block"
        />
        <rect x="10" y="10" width="12" height="12" rx="3" className="fill-brand" />
      </g>
      <g className="fill-foreground">
        <path d="M71.8631 21.5534C71.8631 25.2787 69.5299 27.4266 65.459 27.4266H62.2804V31.9977H58.6666V15.9998H65.459C69.5299 15.9998 71.8631 18.0111 71.8631 21.5534ZM68.4325 21.6676C68.4325 19.8852 67.2886 18.9941 65.2763 18.9941H62.2804V24.4105H65.2763C67.2886 24.4105 68.4325 23.4958 68.4325 21.6676Z" />
        <path d="M77.5266 15.9998V23.999H84.7766V15.9998H88.3883V31.9977H84.7766V27.1982H77.5266V31.9977H73.916V15.9998H77.5266Z" />
        <path d="M102.261 28.798H94.9242L93.6297 31.9998H89.9017L96.8542 16.002H100.582L107.421 31.9998H103.555L102.261 28.798ZM100.974 25.5962L98.6133 19.7711L96.2332 25.5984L100.974 25.5962Z" />
        <path d="M112.686 15.9998V28.8439H119.547V31.9977H109.072V15.9998H112.686Z" />
        <path d="M132.71 28.798H125.373L124.078 31.9998H120.35L127.303 16.002H131.031L137.867 31.9998H134.001L132.71 28.798ZM131.423 25.5962L129.064 19.7689L126.684 25.5962H131.423Z" />
        <path transform="translate(87.15)" d="M71.8631 21.5534C71.8631 25.2787 69.5299 27.4266 65.459 27.4266H62.2804V31.9977H58.6666V15.9998H65.459C69.5299 15.9998 71.8631 18.0111 71.8631 21.5534ZM68.4325 21.6676C68.4325 19.8852 67.2886 18.9941 65.2763 18.9941H62.2804V24.4105H65.2763C67.2886 24.4105 68.4325 23.4958 68.4325 21.6676Z" />
        <path transform="translate(69.32)" d="M102.261 28.798H94.9242L93.6297 31.9998H89.9017L96.8542 16.002H100.582L107.421 31.9998H103.555L102.261 28.798ZM100.974 25.5962L98.6133 19.7711L96.2332 25.5984L100.974 25.5962Z" />
        <path d="M176.19 16H180.056L182.8794 22.9868L185.7267 16H189.4547L184.6856 26.9738V32H181.0731V27.4227Z" />
      </g>
    </svg>
  );
}

// The hero's two calls to action: one height, whatever their variant.
const HERO_BUTTON = "h-10 px-4";

// The headline, with the fact behind each of its words (docs/architecture.md §8, the typical credit
// at depth 2, `typical_credit_seconds`; README.md), and the way to run it: self-hosting on Phala Cloud.
export function Hero() {
  return (
    <section aria-labelledby="hero-title">
      {/* From xl, where both sentences fit beside them, the CTAs sit right of the subline, centred on
          it, under the headline. */}
      <div className={`${CONTAINER} grid justify-items-start gap-3 py-8 xl:grid-cols-[minmax(0,1fr)_auto] xl:items-center xl:gap-x-8`}>
        <h1 id="hero-title" className="max-w-5xl text-3xl leading-tight font-semibold tracking-tight text-balance sm:text-4xl xl:col-span-2">
          {TAGLINE}
        </h1>
        <p className="max-w-2xl text-base text-pretty text-muted-foreground sm:text-lg">
          {/* Each sentence starts a line, and is one where the paragraph is wide; its clauses kept
              whole so it wraps only between them (checked at 360 and 390 px). */}
          <span className="block">
            Credited at two confirmations, <span className="whitespace-nowrap">about 30&nbsp;s on Ethereum</span>,{" "}
            <span className="whitespace-nowrap">in a TEE you can verify</span>.
          </span>
          <span className="block">
            <span className="whitespace-nowrap">Addresses can only pay your treasury</span>,{" "}
            <span className="whitespace-nowrap">and the API follows Stripe's</span>.
          </span>
        </p>
        <div className="flex flex-wrap items-center gap-3 pt-2 xl:pt-0">
          <Button asChild size="lg" className={HERO_BUTTON}>
            <a href={LINKS.deploy}>
              <Rocket aria-hidden="true" />
              Deploy on Phala Cloud
            </a>
          </Button>
          <Button asChild size="lg" variant="outline" className={HERO_BUTTON}>
            <a href={LINKS.docs}>
              <BookOpen aria-hidden="true" />
              Docs
            </a>
          </Button>
        </div>
      </div>
    </section>
  );
}

export function Properties() {
  return (
    <section aria-labelledby="properties-title" className="border-t bg-muted/30">
      <div className={`${CONTAINER} py-16`}>
        <h2 id="properties-title" className="text-2xl font-semibold tracking-tight">
          Payments you can verify
        </h2>
        <p className="mt-2 max-w-2xl leading-6 text-muted-foreground">
          The service never holds funds, and you can check what it runs before you trust it.
        </p>
        <ul className="mt-10 grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          {PROPERTIES.map(({ icon: Icon, title, text }) => (
            <li key={title} className="rounded-xl border bg-card p-6 text-card-foreground">
              <span className="flex size-9 items-center justify-center rounded-lg border bg-background" aria-hidden="true">
                <Icon className="size-4" />
              </span>
              <h3 className="mt-4 text-sm font-semibold">{title}</h3>
              <p className="mt-2 text-sm leading-6 text-pretty text-muted-foreground">{text}</p>
            </li>
          ))}
        </ul>
      </div>
    </section>
  );
}

const FOOTER: { title: string; links: { href: string; label: string }[] }[] = [
  {
    title: "Product",
    links: [
      { href: "#demo", label: "Live demo" },
      { href: LINKS.selfHosting, label: "Self-hosting" },
      { href: LINKS.security, label: "Security" },
    ],
  },
  {
    title: "Developers",
    links: [
      { href: LINKS.docs, label: "Integration guide" },
      { href: LINKS.reference, label: "API reference" },
      { href: LINKS.npm, label: "npm @phala/pay-react" },
    ],
  },
  {
    title: "Open source",
    links: [
      { href: LINKS.repo, label: "GitHub" },
      { href: LINKS.license, label: "Apache-2.0 license" },
    ],
  },
];

export function SiteFooter() {
  return (
    <footer className="border-t">
      <div className={`${CONTAINER} grid gap-10 py-16 text-sm sm:grid-cols-2 lg:grid-cols-[minmax(0,2fr)_repeat(3,minmax(0,1fr))]`}>
        <div>
          <Lockup />
          <p className="mt-3 max-w-xs leading-6 text-muted-foreground">{TAGLINE}</p>
        </div>
        {FOOTER.map((column) => (
          <nav key={column.title} aria-label={column.title}>
            <h2 className="font-medium">{column.title}</h2>
            <ul className="mt-3 flex flex-col gap-2">
              {column.links.map(({ href, label }) => (
                <li key={label}>
                  <a className="text-muted-foreground transition-colors hover:text-foreground" href={href}>
                    {label}
                  </a>
                </li>
              ))}
            </ul>
          </nav>
        ))}
      </div>
      <div className="border-t">
        <div
          className={`${CONTAINER} flex flex-col gap-2 py-6 text-xs text-muted-foreground sm:flex-row sm:items-center sm:justify-between`}
        >
          <p>© 2026 Phala Network</p>
          <p>The demo above runs on testnets with test tokens; no real money moves.</p>
        </div>
      </div>
    </footer>
  );
}
