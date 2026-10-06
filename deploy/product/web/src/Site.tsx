import { ArrowDown, ArrowRight, Menu, Plus, X } from "lucide-react";
import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { CopyButton } from "@/components/ui/hash";
import { CodeBody, CodeWindow, HighlightedLines } from "@/components/code";
import { cn } from "@/lib/utils";
import { useHydrated } from "./islands.js";
import { ComparisonCell } from "./ComparisonCell.js";
import { TEASER, TEASER_OTHERS } from "./content/compare.js";
import { HERO_CODE, HERO_CODE_NOTE } from "./content/hero-code.js";
import {
  CLOSING_LEAD, CLOSING_TITLE, CUSTODY_LINKS, CUSTODY_PATH, DEMO_LEAD, DEMO_STATUS, DEMO_TITLE, DEPLOY_COMMAND, FAQ, HERO_FACTS,
  HERO_META, HERO_SUBHEAD, PROPERTIES, PROPERTIES_LEAD, STEPS, TAGLINE,
} from "./content/site.js";
import { unbroken } from "./text.js";
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
  issues: `${REPO}/issues`,
};

/**
 * The page's grid: one width for every section and the header and footer, so all share a left
 * edge (1280px, 16 to 32px gutters), 12 columns inside it. Text keeps to a readable measure.
 */
export const CONTAINER = "mx-auto w-full max-w-7xl px-4 sm:px-6 lg:px-8";
/** Every H2, and every other page's H1. */
export const H2 = "text-title-sm font-semibold text-balance sm:text-title";
/** A section's introduction under its heading. */
export const LEAD = "text-lead text-pretty text-body-foreground";
/** One rhythm for every section: 80px apart on phones, 112px from lg; below the 64px header when scrolled to. */
export const SECTION = "scroll-mt-16 py-20 lg:py-28";
const ICON = { "aria-hidden": true, strokeWidth: 1.75 } as const;
/** An inline text link, in the text's colour. */
export const TEXT_LINK = "font-medium text-foreground underline decoration-foreground/30 underline-offset-4 transition-colors hover:decoration-foreground";

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
    <header ref={header} className="sticky top-0 z-50 border-b bg-background/90 backdrop-blur-md">
      <div className={`${CONTAINER} flex h-16 items-center`}>
        <a href="/" className="mr-auto flex rounded-md" aria-label="Phala Pay home">
          <Lockup />
        </a>
        <nav aria-label="Site" className="hidden md:block">
          <ul className="flex items-center">
            {NAV.map(({ href, label }) => (
              <li key={label}>
                <a href={href} className="inline-flex h-9 items-center rounded-md px-3 text-sm font-medium text-muted-foreground transition-colors hover:text-foreground">
                  {label}
                </a>
              </li>
            ))}
          </ul>
        </nav>
        <span aria-hidden="true" className="mx-3 hidden h-5 w-px bg-border md:block" />
        <div className="flex items-center gap-1">
          <a href={LINKS.repo} aria-label="GitHub" className={ICON_BUTTON}>
            <span aria-hidden="true" className="github-icon inline-block size-4 shrink-0 bg-current" />
          </a>
          <ThemeToggle theme={theme} onChange={onThemeChange} />
          <Button asChild size="sm" className="ml-3 hidden h-9 md:inline-flex">
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
 * The logo: the mark (brand/README.md: a lime dot in a near-black tile, with its edge on the dark
 * theme) at 24px, beside the name set in the page's typeface, as a product's name sits beside its
 * mark in an interface; the brand's lettered lockup stays for the link preview and the brand files.
 */
export function Lockup({ className }: { className?: string }) {
  return (
    <span className={cn("flex items-center gap-2.5", className)}>
      <svg viewBox="0 0 32 32" aria-hidden="true" className="size-6 shrink-0">
        <rect width="32" height="32" rx="8" className="fill-neutral-950" />
        <path
          fillRule="evenodd"
          d="M8 0H24A8 8 0 0 1 32 8V24A8 8 0 0 1 24 32H8A8 8 0 0 1 0 24V8A8 8 0 0 1 8 0ZM8 1.33A6.67 6.67 0 0 0 1.33 8V24A6.67 6.67 0 0 0 8 30.67H24A6.67 6.67 0 0 0 30.67 24V8A6.67 6.67 0 0 0 24 1.33Z"
          className="hidden fill-white/15 dark:block"
        />
        <rect x="10" y="10" width="12" height="12" rx="3" className="fill-brand" />
      </svg>
      <span className="text-[1.0625rem] font-semibold tracking-[-0.02em] text-foreground">Phala Pay</span>
    </span>
  );
}

// The headline, with the fact behind each of its words (docs/architecture.md §8, the typical credit
// at depth 2, `typical_credit_seconds`; README.md), the way to run it (self-hosting on Phala Cloud),
// and beside them, what integrating it takes.
export function Hero({ code }: { code: ReactNode }) {
  return (
    <section aria-labelledby="hero-title" className="border-b">
      <div className={`${CONTAINER} grid gap-12 pt-14 pb-16 sm:pt-20 lg:grid-cols-12 lg:items-center lg:gap-10 lg:py-24`}>
        <div className="lg:col-span-6">
          <h1 id="hero-title" className="max-w-xl text-display-sm font-semibold text-balance sm:text-display lg:text-display-sm xl:text-display">
            {TAGLINE}
          </h1>
          <p className="mt-6 max-w-xl text-lead text-pretty text-body-foreground">{unbroken(HERO_SUBHEAD)}</p>
          <div className="mt-8 flex flex-col gap-3 sm:flex-row">
            <Button asChild size="lg"><a href={LINKS.deploy}>Start a testnet instance</a></Button>
            <Button asChild size="lg" variant="secondary"><a href={LINKS.docs}>Read the docs</a></Button>
          </div>
          <p className="mt-4 text-sm text-muted-foreground">{HERO_META}</p>
          <dl className="mt-12 grid max-w-xl grid-cols-3 border-t pt-6">
            {HERO_FACTS.map(({ value, label }, index) => (
              <div key={label} className={cn("flex min-w-0 flex-col-reverse justify-end gap-1", index > 0 && "border-l pl-3 min-[360px]:pl-4 sm:pl-6")}>
                <dt className="text-sm text-muted-foreground">{label}</dt>
                <dd className="text-base font-semibold tracking-tight whitespace-nowrap tabular-nums min-[360px]:text-lg sm:text-2xl">{value}</dd>
              </div>
            ))}
          </dl>
        </div>
        <div className="min-w-0 lg:col-span-6">{code}</div>
      </div>
    </section>
  );
}

/**
 * The hero's code, an island of its own: one window, a tab for each side of the integration (the
 * WAI-ARIA tabs pattern: arrow keys move between tabs), the shown one's file name and copy button in
 * its header. Radix Tabs is not used here: it writes style attributes into the prerendered HTML,
 * which the CSP's style-src refuses. Without script the window shows the server's code; the tabs
 * and the copy button work once it hydrates.
 */
export function HeroCode() {
  const hydrated = useHydrated();
  const id = useId();
  const [shown, setShown] = useState(0);
  const tabs = useRef<(HTMLButtonElement | null)[]>([]);
  const current = HERO_CODE[shown] ?? HERO_CODE[0];
  const select = (index: number) => {
    const next = (index + HERO_CODE.length) % HERO_CODE.length;
    setShown(next);
    tabs.current[next]?.focus();
  };
  return (
    <figure className="min-w-0">
      <CodeWindow
        header={
          <>
            <div role="tablist" aria-label="Integration code" className="flex h-full items-stretch gap-5"
              onKeyDown={(event) => {
                if (event.key === "ArrowRight") select(shown + 1);
                else if (event.key === "ArrowLeft") select(shown - 1);
                else return;
                event.preventDefault();
              }}>
              {HERO_CODE.map(({ id: key, label }, index) => (
                <button key={key} ref={(node) => { tabs.current[index] = node; }} type="button" role="tab"
                  id={`${id}-tab-${key}`} aria-controls={`${id}-panel-${key}`} aria-selected={index === shown}
                  tabIndex={index === shown ? 0 : -1} disabled={!hydrated && index !== shown} onClick={() => setShown(index)}
                  className="relative text-sm font-medium text-code-muted transition-colors after:absolute after:inset-x-0 after:bottom-0 after:h-px after:bg-code-foreground after:opacity-0 hover:text-code-foreground focus-visible:outline-2 focus-visible:-outline-offset-2 aria-selected:text-code-foreground aria-selected:after:opacity-100">
                  {label}
                </button>
              ))}
            </div>
            <span className="ml-auto font-mono text-xs text-code-muted">{current?.file}</span>
            {hydrated && current !== undefined
              ? <CopyButton value={current.code} label={`Copy ${current.file}`} className="text-code-muted hover:bg-white/10 hover:text-code-foreground" />
              : <span aria-hidden="true" className="size-8" />}
          </>
        }
      >
        {HERO_CODE.map(({ id: key, file, lines }, index) => (
          <div key={key} role="tabpanel" id={`${id}-panel-${key}`} aria-labelledby={`${id}-tab-${key}`} hidden={index !== shown}>
            {/* Fourteen lines at most: every snippet fits without scrolling down. */}
            <CodeBody label={file} className="min-h-[calc(14lh+2rem)]">
              <HighlightedLines lines={lines} />
            </CodeBody>
          </div>
        ))}
      </CodeWindow>
      <figcaption className="mt-4 text-sm text-muted-foreground">
        {HERO_CODE_NOTE.before}<code className="font-mono text-[0.8125rem] text-foreground">{HERO_CODE_NOTE.code}</code>{HERO_CODE_NOTE.after}
      </figcaption>
    </figure>
  );
}

/** A section's heading and introduction: the heading on the left, the introduction beside it from lg. */
function SectionHeader({ id, title, lead }: { id: string; title: string; lead?: ReactNode }) {
  return (
    <div className="grid gap-4 lg:grid-cols-12 lg:items-end lg:gap-10">
      <div className="lg:col-span-6">
        <h2 id={id} className={H2}>{title}</h2>
      </div>
      {lead !== undefined && <p className={cn(LEAD, "max-w-xl lg:col-span-6 lg:justify-self-end")}>{lead}</p>}
    </div>
  );
}

/**
 * The demo, directly below the hero, on a band of its own: the product itself, sized so that its
 * heading and both panels fit one 1440×900 screen. Until its chunk renders, its placeholder holds
 * the height the demo's first view measures at each breakpoint, so nothing below it moves when it
 * arrives.
 */
export function DemoSection({ children }: { children?: ReactNode }) {
  return (
    <section id="demo" aria-labelledby="demo-title" className="scroll-mt-16 border-b bg-surface py-6 lg:py-7">
      <div className={CONTAINER}>
        <div className="flex flex-wrap items-end justify-between gap-x-10 gap-y-2">
          <div className="flex flex-wrap items-baseline gap-x-4 gap-y-1">
            <h2 id="demo-title" className={H2}>{DEMO_TITLE}</h2>
            <p className="flex items-center gap-2 text-sm font-medium text-muted-foreground">
              <span aria-hidden="true" className="size-2 rounded-full bg-brand ring-1 ring-foreground/25" />
              {DEMO_STATUS}
            </p>
          </div>
          <p className="max-w-md text-pretty text-body-foreground">{DEMO_LEAD}</p>
        </div>
        <div id="demo-root" className="mt-4">{children ?? <DemoPlaceholder />}</div>
      </div>
    </section>
  );
}

// The demo's first view (product and backend, the account loaded) measures 1484px tall at 390px wide
// (1608px at 320, 1399px at 500), 1229 to 1265px from 640px, and 576 to 632px from 1024px, where
// its columns sit side by side. Without scripting the demo never arrives, so nothing is reserved.
const DEMO_HEIGHT = "min-h-[93rem] sm:min-h-[77rem] lg:min-h-[36rem] noscript:min-h-0";

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

/**
 * The three steps along one rule, each with what the integration writes for it. On a phone they
 * stack along a rule down their left.
 */
export function HowItWorks() {
  return (
    <section aria-labelledby="how-title" className={SECTION}>
      <div className={CONTAINER}>
        <SectionHeader id="how-title" title="How it works" lead="Three calls in your backend and one component in your page. Phala Pay never holds the funds." />
        <ol className="mt-14 grid gap-10 md:grid-cols-3 md:gap-8">
          {STEPS.map(({ title, text, code }, index) => (
            <li key={title} className="relative flex flex-col items-start pl-12 md:pt-12 md:pl-0">
              {/* The rule through the numbers: across the row from md, down the left on a phone. */}
              {index < STEPS.length - 1 && (
                <span aria-hidden="true" className="absolute top-8 bottom-[-2.5rem] left-[0.9375rem] w-px bg-border md:top-[0.9375rem] md:right-[-2rem] md:bottom-auto md:left-8 md:h-px md:w-auto" />
              )}
              <span aria-hidden="true" className="absolute top-0 left-0 flex size-8 items-center justify-center rounded-full border bg-background font-mono text-sm text-foreground">
                {index + 1}
              </span>
              <h3 className="text-heading font-semibold">{title}</h3>
              <p className="mt-2 mb-4 max-w-sm text-pretty text-body-foreground">{unbroken(text)}</p>
              <code className="mt-auto inline-flex rounded-md border bg-muted/60 px-2 py-1 font-mono text-[0.8125rem] text-foreground">{code}</code>
            </li>
          ))}
        </ol>
      </div>
    </section>
  );
}

/** Where a payment can go: the customer's wallet, the deposit contract, and only then the treasury. */
function CustodyPath() {
  return (
    <figure>
      <ol className="grid gap-2 md:grid-cols-[1fr_auto_1fr_auto_1fr] md:gap-3">
        {CUSTODY_PATH.map(({ name, detail }, index) => (
          <li key={name} className="contents">
            {index > 0 && (
              <span className="flex items-center gap-2 pl-4 text-xs font-medium text-muted-foreground md:flex-col md:justify-center md:gap-1 md:pl-0">
                <ArrowDown {...ICON} className="size-4 md:hidden" />
                <ArrowRight {...ICON} className="hidden size-4 md:block" />
                {CUSTODY_LINKS[index - 1]}
              </span>
            )}
            <div className={cn("rounded-lg border bg-card px-4 py-3", index === CUSTODY_PATH.length - 1 && "border-foreground/40")}>
              <p className="text-sm font-semibold">{name}</p>
              <p className="mt-0.5 text-sm text-muted-foreground">{detail}</p>
            </div>
          </li>
        ))}
      </ol>
      <figcaption className="mt-4 text-sm text-muted-foreground">The operator holds no key to the funds and cannot change the treasury.</figcaption>
    </figure>
  );
}

/**
 * Why Phala Pay: the heading and its introduction, then custody, the first property, beside the path a
 * payment takes, and under them the other four in a row.
 */
export function Properties() {
  const [custody, ...rest] = PROPERTIES;
  return (
    <section aria-labelledby="properties-title" className={cn(SECTION, "border-t")}>
      <div className={CONTAINER}>
        <SectionHeader id="properties-title" title="Why Phala Pay" lead={PROPERTIES_LEAD} />
        {custody !== undefined && (
          <div className="mt-12 grid gap-8 border-y py-10 lg:grid-cols-12 lg:items-center lg:gap-10">
            <div className="lg:col-span-4">
              <h3 className="text-heading font-semibold">{custody.title}</h3>
              <p className="mt-2 text-pretty text-body-foreground">{unbroken(custody.text)}</p>
            </div>
            <div className="lg:col-span-8">
              <CustodyPath />
            </div>
          </div>
        )}
        <dl className="mt-10 grid gap-x-10 gap-y-10 sm:grid-cols-2 lg:grid-cols-4">
          {rest.map(({ title, text }) => (
            <div key={title}>
              <dt className="text-heading font-semibold">{title}</dt>
              <dd className="mt-2 text-pretty text-body-foreground">{unbroken(text)}</dd>
            </div>
          ))}
        </dl>
      </div>
    </section>
  );
}

/** Names in prose: "A, B, and C". */
const list = new Intl.ListFormat("en", { type: "conjunction" });

/**
 * The comparison's summary: from md a table, Phala Pay's column marked by a rule above it; on a
 * phone, each provider in turn with its four values.
 */
export function CompareTeaser() {
  const [phala, ...others] = TEASER.vendors;
  return (
    <section aria-labelledby="compare-title" className={cn(SECTION, "border-t")}>
      <div className={CONTAINER}>
        <SectionHeader
          id="compare-title"
          title="How Phala Pay compares"
          lead={<>Beside {list.format(others.map(({ name }) => name))}, as each states it. The full comparison adds {list.format(TEASER_OTHERS)}, with a source for every value.</>}
        />
        <div className="mt-12 hidden md:block">
          <table className="w-full table-fixed border-collapse text-left">
            <caption className="sr-only">{list.format(TEASER.vendors.map(({ name }) => name))} on {TEASER.dimensions.length} dimensions.</caption>
            <thead>
              <tr>
                <td className="w-44 lg:w-56" />
                {TEASER.vendors.map(({ id, name }) => (
                  <th key={id} scope="col" className={cn("border-t-2 px-5 pt-4 pb-4 align-bottom text-base font-semibold", id === phala?.id ? "border-foreground" : "border-transparent text-body-foreground")}>
                    {name}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {TEASER.dimensions.map(({ key, label }) => (
                <tr key={key} className="border-t">
                  <th scope="row" className="py-5 pr-5 align-top text-sm font-medium text-muted-foreground">{label}</th>
                  {TEASER.vendors.map((vendor) => (
                    <td key={vendor.id} className={cn("px-5 py-5 align-top text-[0.9375rem]/6 text-pretty", vendor.id === phala?.id ? "text-foreground" : "text-body-foreground")}>
                      <ComparisonCell cell={vendor[key]} linkSource={false} />
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <ProviderList vendors={TEASER.vendors} dimensions={TEASER.dimensions} linkSource={false} className="mt-10 md:hidden" />
        <a href="/compare" className={cn(TEXT_LINK, "mt-8 inline-flex min-h-11 items-center gap-1.5 text-sm")}>
          See the full comparison
        </a>
      </div>
    </section>
  );
}

/**
 * Below md, the comparison per provider: each its name, then its values beside their dimensions,
 * Phala Pay first. Read top to bottom, one provider at a time.
 */
export function ProviderList({ vendors, dimensions, linkSource, className }: {
  vendors: typeof TEASER.vendors;
  dimensions: typeof TEASER.dimensions;
  linkSource: boolean;
  className?: string;
}) {
  return (
    <div className={cn("flex flex-col gap-10", className)}>
      {vendors.map((vendor, index) => (
        <section key={vendor.id} id={linkSource ? `provider-${vendor.id}` : undefined} aria-labelledby={`${linkSource ? "page" : "teaser"}-provider-${vendor.id}`} className="scroll-mt-20">
          <h3 id={`${linkSource ? "page" : "teaser"}-provider-${vendor.id}`} className={cn("border-t-2 pt-3 text-heading font-semibold", index === 0 ? "border-foreground" : "border-border")}>
            {vendor.name}
          </h3>
          <dl className="mt-2 divide-y">
            {dimensions.map(({ key, label }) => (
              <div key={key} className="grid grid-cols-[7.5rem_minmax(0,1fr)] gap-4 py-3 text-sm">
                <dt className="text-muted-foreground">{label}</dt>
                <dd className={index === 0 ? "text-foreground" : "text-body-foreground"}><ComparisonCell cell={vendor[key]} linkSource={linkSource} /></dd>
              </div>
            ))}
          </dl>
        </section>
      ))}
    </div>
  );
}

/** Each answer folds under its question, natively: no script, so it works before and without hydration. */
export function Faq() {
  return (
    <section aria-labelledby="faq-title" className={cn(SECTION, "border-t")}>
      <div className={`${CONTAINER} grid gap-10 lg:grid-cols-12`}>
        <div className="lg:col-span-4">
          <h2 id="faq-title" className={H2}>Frequently asked questions</h2>
          <p className="mt-4 max-w-sm text-pretty text-body-foreground">
            Not answered here? Read the <a className={TEXT_LINK} href={LINKS.docs}>documentation</a> or ask
            on <a className={TEXT_LINK} href={LINKS.issues}>GitHub</a>.
          </p>
        </div>
        <div className="border-t lg:col-span-8">
          {FAQ.map(({ question, answer }) => (
            <details key={question} className="group border-b">
              <summary className="flex min-h-16 cursor-pointer list-none items-center justify-between gap-6 py-4 text-base font-medium sm:text-lg [&::-webkit-details-marker]:hidden">
                {question}
                <Plus {...ICON} className="size-5 shrink-0 text-muted-foreground group-open:rotate-45 motion-safe:transition-transform" />
              </summary>
              <p className="max-w-2xl pr-10 pb-6 text-pretty text-body-foreground">{unbroken(answer)}</p>
            </details>
          ))}
        </div>
      </div>
    </section>
  );
}

/**
 * The close: a dark panel in either theme with the call to deploy, and beside it the one command
 * that does.
 */
export function ClosingCta({ command }: { command: ReactNode }) {
  return (
    <section aria-labelledby="closing-title" className="pb-20 lg:pb-28">
      <div className={CONTAINER}>
        <div className="dark grid gap-10 rounded-xl border bg-background px-6 py-12 text-foreground sm:px-10 lg:grid-cols-12 lg:items-center lg:gap-10 lg:px-14 lg:py-16">
          <div className="lg:col-span-6">
            <h2 id="closing-title" className={H2}>{CLOSING_TITLE}</h2>
            <p className={cn(LEAD, "mt-4 max-w-md")}>{CLOSING_LEAD}</p>
            <div className="mt-8 flex flex-col gap-3 sm:flex-row">
              <Button asChild size="lg"><a href={LINKS.deploy}>Start a testnet instance</a></Button>
              <Button asChild size="lg" variant="secondary"><a href={LINKS.repo}>View on GitHub</a></Button>
            </div>
          </div>
          <div className="min-w-0 lg:col-span-6">
            {command}
            <p className="mt-3 text-sm text-pretty text-muted-foreground">
              Deploys the latest release. To verify the release's provenance first, follow
              the <a className={TEXT_LINK} href={LINKS.deploy}>high-assurance path</a>.
            </p>
          </div>
        </div>
      </div>
    </section>
  );
}

/** The one-command deploy, in a terminal line, an island for its copy button. */
export function DeployCommand() {
  const hydrated = useHydrated();
  return (
    <div className="rounded-lg border bg-code">
      <div className="flex h-11 items-center gap-3 border-b pr-2 pl-4">
        <span className="text-sm text-muted-foreground">Terminal</span>
        {hydrated ? <CopyButton value={DEPLOY_COMMAND} label="Copy the deploy command" className="ml-auto" /> : <span aria-hidden="true" className="ml-auto size-8" />}
      </div>
      <pre tabIndex={0} role="region" aria-label="Deploy command" className="overflow-x-auto px-4 py-4 font-mono text-[13px]/[1.7] text-code-foreground">
        <code><span aria-hidden="true" className="text-code-muted select-none">$ </span>{DEPLOY_COMMAND}</code>
      </pre>
    </div>
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
      <div className={`${CONTAINER} grid gap-12 pt-14 pb-10 text-sm lg:grid-cols-12 lg:gap-10 lg:pt-16`}>
        <div className="lg:col-span-4">
          <Lockup />
          <p className="mt-4 max-w-xs text-pretty text-muted-foreground">{TAGLINE}. Open source, self-hosted, on Ethereum and Base.</p>
        </div>
        <div className="grid grid-cols-2 gap-x-6 gap-y-10 sm:grid-cols-4 lg:col-span-8">
          {FOOTER.map((column) => (
            <nav key={column.title} aria-label={column.title}>
              <p className="font-medium text-foreground">{column.title}</p>
              {/* Rows 44px tall for touch, 32px from md. */}
              <ul className="mt-3">
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
        <p className="border-t pt-8 text-muted-foreground lg:col-span-12">© 2026 Phala Network</p>
      </div>
    </footer>
  );
}
