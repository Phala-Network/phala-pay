import { ChevronDown, Menu, X } from "lucide-react";
import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { CodeBlock } from "@/components/ui/code-block";
import { CopyButton } from "@/components/ui/hash";
import { Table, TableBody, TableCaption, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { useHydrated } from "./islands.js";
import { HOSTED_PROCESSORS, TEASER } from "./content/compare.js";
import {
  CLOSING_LEAD, CLOSING_TITLE, DEMO_LEAD, DEMO_TITLE, FAQ, HERO_CODE, HERO_CODE_NOTE, HERO_META, HERO_SUBHEAD,
  PROPERTIES, PROPERTIES_LEAD, STEPS, TAGLINE,
} from "./content/site.js";
import { ICON_BUTTON, ThemeToggle, type Theme } from "./theme.js";

export const REPO = "https://github.com/Phala-Network/phala-pay";
export const LINKS = {
  repo: REPO,
  docs: `${REPO}#documentation`,
  overview: `${REPO}/blob/main/docs/overview.md`,
  integration: `${REPO}/blob/main/docs/integration.md`,
  selfHosting: `${REPO}/blob/main/docs/self-hosting.md`,
  // The guide's one-command deploy to your own Phala Cloud workspace, beside its other two paths.
  deploy: `${REPO}/blob/main/docs/self-hosting.md#one-command-deploy`,
  reference: "https://phala-network.github.io/phala-pay/",
  license: `${REPO}/blob/main/LICENSE`,
  security: `${REPO}/blob/main/SECURITY.md`,
};

/**
 * The page's width, one for every section and the header and footer, so all share a left edge; text
 * inside it keeps to a readable measure (max-w-prose and narrower).
 */
export const CONTAINER = "mx-auto w-full max-w-7xl px-4 sm:px-6 lg:px-8";
export const H2 = "text-2xl font-semibold tracking-tight";
/** Below the 56px sticky header, with room above the heading scrolled to. */
const SECTION = "scroll-mt-20 pt-16 md:pt-24";
const ICON = { "aria-hidden": true, strokeWidth: 1.75 } as const;

const NAV = [
  { href: "/#demo", label: "Demo" },
  { href: "/compare", label: "Compare" },
  { href: LINKS.docs, label: "Docs" },
  { href: LINKS.reference, label: "API reference" },
];

export function SiteHeader({ theme, onThemeChange }: { theme: Theme; onThemeChange: (theme: Theme) => void }) {
  const [menuOpen, setMenuOpen] = useState(false);
  const menuId = useId();
  const hydrated = useHydrated();
  const header = useRef<HTMLElement>(null);
  const menuToggle = useRef<HTMLButtonElement>(null);

  // The open menu closes on Escape (returning focus to its button) and on a press outside the header.
  useEffect(() => {
    if (!menuOpen) return;
    const onPointerDown = (event: PointerEvent) => {
      if (event.target instanceof Node && header.current?.contains(event.target)) return;
      setMenuOpen(false);
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      setMenuOpen(false);
      menuToggle.current?.focus();
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [menuOpen]);

  return (
    <header ref={header} className="sticky top-0 z-50 border-b bg-background">
      <div className={`${CONTAINER} flex h-14 items-center gap-6`}>
        <a href="/" className="mr-auto flex rounded-md">
          <Lockup />
        </a>
        <nav aria-label="Site" className="hidden md:block">
          <ul className="flex items-center gap-1">
            {NAV.map(({ href, label }) => (
              <li key={label}>
                <a href={href} className="inline-flex h-8 items-center rounded-md px-3 text-sm text-muted-foreground transition-colors hover:text-foreground">
                  {label}
                </a>
              </li>
            ))}
          </ul>
        </nav>
        <div className="flex items-center gap-2">
          <a href={LINKS.repo} aria-label="GitHub" className={ICON_BUTTON}>
            <span aria-hidden="true" className="github-icon inline-block size-4 shrink-0 bg-current" />
          </a>
          <ThemeToggle theme={theme} onChange={onThemeChange} />
          <Button asChild size="sm" className="ml-2 hidden md:inline-flex">
            <a href={LINKS.selfHosting}>Self-host</a>
          </Button>
          {hydrated && (
            <button ref={menuToggle} type="button" className={`${ICON_BUTTON} md:hidden`} aria-label="Menu"
              aria-expanded={menuOpen} aria-controls={menuId} onClick={() => setMenuOpen((open) => !open)}>
              {menuOpen ? <X {...ICON} /> : <Menu {...ICON} />}
            </button>
          )}
        </div>
      </div>
      {hydrated && (
        <div id={menuId} hidden={!menuOpen} className="absolute inset-x-0 top-full border-b bg-background md:hidden">
          <nav aria-label="Menu" className={CONTAINER}>
            <ul className="divide-y">
              {[...NAV, { href: LINKS.selfHosting, label: "Self-host" }].map(({ href, label }) => (
                <li key={label}>
                  <a href={href} className="flex h-12 items-center text-base font-medium" onClick={() => setMenuOpen(false)}>
                    {label}
                  </a>
                </li>
              ))}
            </ul>
          </nav>
        </div>
      )}
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

// The headline, with the fact behind each of its words (docs/architecture.md §8, the typical credit
// at depth 2, `typical_credit_seconds`; README.md), the way to run it (self-hosting on Phala Cloud),
// and beside them, what integrating it takes.
export function Hero() {
  return (
    <section aria-labelledby="hero-title">
      <div className={`${CONTAINER} grid gap-12 py-12 md:py-20 lg:grid-cols-12 lg:items-center lg:gap-8`}>
        <div className="lg:col-span-7">
          <h1 id="hero-title" className="max-w-2xl text-3xl/tight font-semibold tracking-tight text-balance sm:text-4xl/tight">
            {TAGLINE}
          </h1>
          <p className="mt-4 max-w-xl text-lg/8 text-pretty text-muted-foreground">{HERO_SUBHEAD}</p>
          <div className="mt-8 flex flex-col gap-3 sm:flex-row">
            <Button asChild size="lg"><a href={LINKS.deploy}>Start a testnet instance</a></Button>
            <Button asChild size="lg" variant="secondary"><a href={LINKS.docs}>Read the docs</a></Button>
          </div>
          <p className="mt-6 text-sm text-muted-foreground">{HERO_META}</p>
        </div>
        <div id="hero-code" className="min-w-0 lg:col-span-5">
          <HeroCode />
        </div>
      </div>
    </section>
  );
}

/**
 * The hero's code, an island of its own. Each snippet's copy button sits in its caption, clear of
 * lines that scroll on a phone, and appears once the island hydrates.
 */
export function HeroCode() {
  const hydrated = useHydrated();
  return (
    <div className="grid gap-5">
      {HERO_CODE.map(({ label, file, code }) => (
        <figure key={file} className="min-w-0">
          <figcaption className="mb-2 flex h-8 items-center gap-3 text-sm">
            <span className="mr-auto font-medium">{label}</span>
            <span className="font-mono text-xs text-muted-foreground">{file}</span>
            {hydrated && <CopyButton value={code} label={`Copy ${file}`} />}
          </figcaption>
          <CodeBlock value={code} label={file} copyable={false} className="max-h-none pr-3 text-[13px]" />
        </figure>
      ))}
      <p className="text-sm text-muted-foreground">
        {HERO_CODE_NOTE.before}<code className="font-mono text-[13px]">{HERO_CODE_NOTE.code}</code>{HERO_CODE_NOTE.after}
      </p>
    </div>
  );
}

/**
 * The demo, directly below the hero. Until its chunk renders, its placeholder holds the height the
 * demo's first view measures at each breakpoint, so nothing below it moves when it arrives.
 */
export function DemoSection({ children }: { children?: ReactNode }) {
  return (
    <section id="demo" aria-labelledby="demo-title" className="scroll-mt-20">
      <div className={CONTAINER}>
        <h2 id="demo-title" className={H2}>{DEMO_TITLE}</h2>
        <p className="mt-2 max-w-prose text-pretty text-muted-foreground">{DEMO_LEAD}</p>
        <div id="demo-root" className="mt-8">{children ?? <DemoPlaceholder />}</div>
      </div>
    </section>
  );
}

// The demo's first view (product and backend, the account loaded) measures 1830px tall at 390px wide
// (1894px at 320), 1672 to 1692px from 640px, and 1025px from 1024px, where its columns sit side by side.
const DEMO_HEIGHT = "min-h-[114rem] sm:min-h-[105rem] lg:min-h-[64rem]";

/** The demo's space in static HTML and while the page hydrates. */
export function DemoPlaceholder() {
  return (
    <div className={DEMO_HEIGHT}>
      <noscript>
        <p className="text-sm text-muted-foreground">The demo needs JavaScript.</p>
      </noscript>
    </div>
  );
}

/** The demo's space while its chunk loads. */
export function DemoLoading() {
  return (
    <div className={DEMO_HEIGHT}>
      <p role="status" className="text-sm text-muted-foreground">Loading the demo…</p>
    </div>
  );
}

export function HowItWorks() {
  return (
    <section aria-labelledby="how-title" className={SECTION}>
      <div className={CONTAINER}>
        <h2 id="how-title" className={H2}>How it works</h2>
        <ol className="mt-8 grid gap-8 md:grid-cols-3 md:gap-6">
          {STEPS.map(({ title, text }, index) => (
            <li key={title} className="border-l pl-5">
              <span aria-hidden="true" className="font-mono text-sm text-muted-foreground">
                {String(index + 1).padStart(2, "0")}
              </span>
              <h3 className="mt-2 font-semibold">{title}</h3>
              <p className="mt-1 max-w-prose text-sm/6 text-pretty text-muted-foreground">{text}</p>
            </li>
          ))}
        </ol>
      </div>
    </section>
  );
}

export function Properties() {
  return (
    <section aria-labelledby="properties-title" className={SECTION}>
      <div className={`${CONTAINER} grid gap-8 lg:grid-cols-12`}>
        <div className="lg:col-span-4">
          <div className="lg:sticky lg:top-20">
            <h2 id="properties-title" className={H2}>Why Phala Pay</h2>
            <p className="mt-2 max-w-sm text-pretty text-muted-foreground">{PROPERTIES_LEAD}</p>
          </div>
        </div>
        <dl className="divide-y border-y lg:col-span-8">
          {PROPERTIES.map(({ title, text }) => (
            <div key={title} className="grid gap-1 py-5 sm:grid-cols-3 sm:gap-6">
              <dt className="font-semibold">{title}</dt>
              <dd className="text-sm/6 text-pretty text-muted-foreground sm:col-span-2">{text}</dd>
            </div>
          ))}
        </dl>
      </div>
    </section>
  );
}

export function CompareTeaser() {
  return (
    <section aria-labelledby="compare-title" className={SECTION}>
      <div className={CONTAINER}>
        <h2 id="compare-title" className={H2}>How Phala Pay compares</h2>
        <p className="mt-2 max-w-prose text-pretty text-muted-foreground">
          Beside hosted processors ({HOSTED_PROCESSORS}) and the self-hosted BTCPay Server.
        </p>
        <Table className="mt-8 hidden table-fixed md:table">
          <TableCaption className="sr-only">Phala Pay, hosted processors, and BTCPay Server on four dimensions.</TableCaption>
          <TableHeader>
            <TableRow className="hover:bg-transparent">
              <TableHead scope="col" className="w-40 pl-0 text-xs text-muted-foreground">Dimension</TableHead>
              {TEASER.columns.map((column, index) => (
                <TableHead key={column} scope="col" className={`px-4 ${index === 0 ? "bg-muted/40" : ""}`}>{column}</TableHead>
              ))}
            </TableRow>
          </TableHeader>
          <TableBody>
            {TEASER.rows.map(({ label, cells }) => (
              <TableRow key={label}>
                <TableHead scope="row" className="h-auto py-3 pl-0 align-top">{label}</TableHead>
                {cells.map((cell, index) => (
                  <TableCell key={index} className={`px-4 py-3 align-top whitespace-normal ${index === 0 ? "bg-muted/40" : "text-muted-foreground"}`}>
                    {cell}
                  </TableCell>
                ))}
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {/* Below md, one list per dimension instead of a table four columns wide. */}
        <dl className="mt-6 divide-y border-y md:hidden">
          {TEASER.rows.map(({ label, cells }) => (
            <div key={label} className="py-4">
              <dt className="font-semibold">{label}</dt>
              <dd>
                <dl className="mt-2 grid gap-3 text-sm">
                  {TEASER.columns.map((column, index) => (
                    <div key={column}>
                      <dt className="text-muted-foreground">{column}</dt>
                      <dd>{cells[index]}</dd>
                    </div>
                  ))}
                </dl>
              </dd>
            </div>
          ))}
        </dl>
        <a href="/compare" className="mt-4 inline-flex min-h-11 items-center text-sm font-medium underline underline-offset-4">
          Full comparison
        </a>
      </div>
    </section>
  );
}

/** Each answer folds under its question, natively: no script, so it works before and without hydration. */
export function Faq() {
  return (
    <section aria-labelledby="faq-title" className={SECTION}>
      <div className={`${CONTAINER} grid gap-8 lg:grid-cols-12`}>
        <h2 id="faq-title" className={`${H2} lg:col-span-4`}>Frequently asked questions</h2>
        <div className="border-t lg:col-span-8">
          {FAQ.map(({ question, answer }) => (
            <details key={question} className="group border-b">
              <summary className="flex min-h-12 cursor-pointer list-none items-center justify-between gap-4 py-3 font-medium [&::-webkit-details-marker]:hidden">
                {question}
                <ChevronDown {...ICON} className="size-4 shrink-0 text-muted-foreground group-open:rotate-180 motion-safe:transition-transform" />
              </summary>
              <p className="max-w-prose pb-5 text-sm/6 text-pretty text-muted-foreground">{answer}</p>
            </details>
          ))}
        </div>
      </div>
    </section>
  );
}

export function ClosingCta() {
  return (
    <section aria-labelledby="closing-title" className={SECTION}>
      <div className={CONTAINER}>
        {/* Ruled above; the footer's rule closes it below. */}
        <div className="flex flex-col gap-6 border-t py-10 md:flex-row md:items-center md:justify-between md:py-12">
          <div>
            <h2 id="closing-title" className={H2}>{CLOSING_TITLE}</h2>
            <p className="mt-2 text-pretty text-muted-foreground">{CLOSING_LEAD}</p>
          </div>
          <div className="flex shrink-0 flex-col gap-3 sm:flex-row">
            <Button asChild size="lg"><a href={LINKS.deploy}>Start a testnet instance</a></Button>
            <Button asChild size="lg" variant="secondary"><a href={LINKS.repo}>View on GitHub</a></Button>
          </div>
        </div>
      </div>
    </section>
  );
}

const FOOTER: { title: string; links: { href: string; label: string }[] }[] = [
  {
    title: "Product",
    links: [
      { href: "/#demo", label: "Demo" },
      { href: "/compare", label: "Compare" },
      { href: LINKS.overview, label: "How it works" },
      { href: LINKS.selfHosting, label: "Self-hosting" },
      { href: LINKS.security, label: "Security" },
    ],
  },
  {
    title: "Developers",
    links: [
      { href: LINKS.docs, label: "Documentation" },
      { href: LINKS.integration, label: "Integration guide" },
      { href: LINKS.reference, label: "API reference" },
    ],
  },
  {
    title: "Packages",
    links: [
      { href: "https://www.npmjs.com/package/@phala/pay-react", label: "@phala/pay-react" },
      { href: "https://www.npmjs.com/package/@phala/pay", label: "@phala/pay" },
      { href: "https://www.npmjs.com/package/@phala/pay-server", label: "@phala/pay-server" },
      { href: "https://pypi.org/project/phala-pay/", label: "phala-pay (Python)" },
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
      <div className={`${CONTAINER} grid gap-10 py-12 text-sm md:py-16 lg:grid-cols-12 lg:gap-8`}>
        <div className="lg:col-span-4">
          <Lockup />
          <p className="mt-4 text-muted-foreground">© 2026 Phala Network</p>
        </div>
        <div className="grid grid-cols-2 gap-x-6 gap-y-10 sm:grid-cols-4 lg:col-span-8">
          {FOOTER.map((column) => (
            <nav key={column.title} aria-label={column.title}>
              <p className="font-medium">{column.title}</p>
              {/* Rows 44px tall for touch, 32px from md. */}
              <ul className="mt-2">
                {column.links.map(({ href, label }) => (
                  <li key={label}>
                    <a className="inline-flex min-h-11 items-center text-muted-foreground transition-colors hover:text-foreground md:min-h-8" href={href}>
                      {label}
                    </a>
                  </li>
                ))}
              </ul>
            </nav>
          ))}
        </div>
      </div>
    </footer>
  );
}
