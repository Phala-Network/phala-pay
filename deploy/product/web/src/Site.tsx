import { ArrowRight, Check, ChevronRight, FileCode2, Landmark, Menu, MoveDown, Plus, ShieldCheck, Wallet, X, type LucideIcon } from "lucide-react";
import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { CopyButton } from "@/components/ui/copy-button";
import { CodeBody, CodeWindow, HighlightedLines } from "@/components/code";
import { cn } from "@/lib/utils";
import { ASIDE, GRID, LEFT, MAIN, RIGHT } from "./layout.js";
import { useHydrated } from "./islands.js";
import { ComparisonCell } from "./ComparisonCell.js";
import { Versus } from "./Versus.js";
import { TEASER, TEASER_OTHERS } from "./content/compare.js";
import { HERO_CODE, HERO_CODE_NOTE } from "./content/hero-code.js";
import {
  CLOSING_LEAD, CLOSING_TITLE, CREDIT_TIMES, CUSTODY_LINKS, CUSTODY_NOTE, CUSTODY_PATH, DEMO_LEAD, DEMO_STATUS, DEMO_TITLE,
  DEPLOY_COMMAND, EYEBROWS, FAQ, FEATURES_TITLE, FEE_FIGURE, HERO_FACTS, HERO_FACTS_NOTE, HERO_META, HERO_SUBHEAD, MONEY_TITLE, PROPERTIES,
  PROPERTIES_LEAD, SDK_PACKAGES, TAGLINE, VERIFY_CHECKS, WEBHOOK_EVENTS, type Station,
} from "./content/site.js";
import { unbroken } from "./text.js";
import { ICON_BUTTON, ThemeToggle, type Theme } from "./theme.js";

export const REPO = "https://github.com/Phala-Network/phala-pay";
export const LINKS = {
  repo: REPO,
  // The docs and the API reference, rendered on this site from the repository (/docs, /reference).
  docs: "/docs",
  overview: "/docs/overview",
  integration: "/docs/integration",
  selfHosting: "/docs/self-hosting",
  // The guide's one-command deploy to your own Phala Cloud workspace, beside its other two paths.
  deploy: "/docs/self-hosting#one-command-deploy",
  reference: "/reference",
  changelog: "/docs/changelog",
  license: `${REPO}/blob/main/LICENSE`,
  security: `${REPO}/blob/main/SECURITY.md`,
  issues: `${REPO}/issues`,
};

/**
 * The page's width: one for every section and the header and footer, so all share a left edge
 * (1280px, 16 to 32px gutters); the 12-column grid inside it is src/layout.ts. From xl its two
 * edges are drawn as rules down the whole page, so every block visibly sits in one frame.
 */
export const CONTAINER = "mx-auto w-full max-w-7xl px-4 sm:px-6 lg:px-8 xl:border-x";
/** Every H2, and every other page's H1. */
export const H2 = "text-title-sm font-semibold text-balance sm:text-title";
/** A section's introduction under its heading. */
export const LEAD = "text-lead text-pretty text-body-foreground";
/** One rhythm for every section: 80px above and below on phones, 112px from lg. */
export const SECTION = "py-20 lg:py-28";
/**
 * A band set apart in the theme's own tone: a faint lime-tinted neutral in the light theme, a step
 * above the page in the dark. Every section stays in its theme; variety comes from the layout, the
 * surface, and the rails.
 */
export const BAND = "border-b bg-band";
const ICON = { "aria-hidden": true, strokeWidth: 1.75 } as const;
/** An inline text link, in the text's colour. */
export const TEXT_LINK = "font-medium text-foreground underline decoration-foreground/30 underline-offset-4 transition-colors hover:decoration-foreground";
/** An arrow that leads a link or button on, nudged on hover where motion is welcome. */
const NUDGE = "motion-safe:transition-transform motion-safe:group-hover:translate-x-0.5";

/**
 * The label above a heading: Geist Mono, uppercase, after a small square in the brand's colour (the
 * mark's inner square).
 */
export function Eyebrow({ children, className }: { children: ReactNode; className?: string }) {
  return (
    // In the line, not a flex row: the label's baseline is its text's, as the grid's alignment reads it.
    <p className={cn("mono-label text-muted-foreground", className)}>
      <span aria-hidden="true" className="mr-2 inline-block size-1.5 bg-brand-ink align-middle" />
      {children}
    </p>
  );
}

/** A section's label, heading, and introduction, stacked; centred in a band that centres its content. */
function Intro({ id, eyebrow, title, lead, align = "start", className }: {
  id: string; eyebrow: string; title: string; lead?: ReactNode; align?: "start" | "center"; className?: string;
}) {
  return (
    <div className={cn("max-w-2xl", align === "center" && "mx-auto text-center", className)}>
      <Eyebrow>{eyebrow}</Eyebrow>
      <h2 id={id} className={cn(H2, "mt-4")}>{unbroken(title)}</h2>
      {lead !== undefined && <p className={cn(LEAD, "mt-5")}>{lead}</p>}
    </div>
  );
}

/** The part of the site a page belongs to, which the header marks as current. */
export type Section = "compare" | "docs" | "reference";

export function isSection(value: string | undefined): value is Section {
  return value === "compare" || value === "docs" || value === "reference";
}

const NAV: { href: string; label: string; section?: Section }[] = [
  { href: "/#demo", label: "Demo" },
  { href: "/compare", label: "Compare", section: "compare" },
  { href: LINKS.docs, label: "Docs", section: "docs" },
  { href: LINKS.reference, label: "API reference", section: "reference" },
];

export function SiteHeader({ theme, onThemeChange, current }: { theme: Theme; onThemeChange: (theme: Theme) => void; current: Section | null }) {
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
    // The site's links beside the mark, the utilities and the one action at the other end.
    <header ref={header} className="sticky top-0 z-50 border-b bg-background/85 backdrop-blur-md">
      <div className={`${CONTAINER} flex h-16 items-center`}>
        <a href="/" className="flex rounded-md" aria-label="Phala Pay home">
          <Lockup />
        </a>
        <nav aria-label="Site" className="ml-10 hidden md:block">
          <ul className="flex items-center gap-1">
            {NAV.map(({ href, label, section }) => (
              <li key={label}>
                <a href={href} aria-current={section !== undefined && section === current ? "page" : undefined}
                  className="inline-flex h-9 items-center rounded-md px-3 text-sm font-medium text-muted-foreground transition-colors hover:text-foreground aria-[current=page]:bg-muted aria-[current=page]:text-foreground">
                  {label}
                </a>
              </li>
            ))}
          </ul>
        </nav>
        <div className="ml-auto flex items-center gap-1">
          <a href={LINKS.repo} aria-label="GitHub" className={ICON_BUTTON}>
            <span aria-hidden="true" className="github-icon inline-block size-4 shrink-0 bg-current" />
          </a>
          <ThemeToggle theme={theme} onChange={onThemeChange} />
          <span aria-hidden="true" className="mx-2 hidden h-5 w-px bg-border md:block" />
          <Button asChild size="sm" className="hidden h-9 md:inline-flex">
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
              {[...NAV, { href: LINKS.selfHosting, label: "Self-host", section: undefined }].map(({ href, label, section }) => (
                <li key={label}>
                  <a href={href} aria-current={section !== undefined && section === current ? "page" : undefined}
                    className="flex h-12 items-center text-base font-medium aria-[current=page]:underline aria-[current=page]:decoration-2 aria-[current=page]:underline-offset-8" onClick={() => setMenuOpen(false)}>
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
 * The logo: the mark (brand/README.md: a lime square in a near-black tile, with its edge on the dark
 * theme) at 24px, beside the name set in the page's typeface, as a product's name sits beside its
 * mark in an interface; the brand's lettered lockup stays for the link preview and the brand files.
 */
export function Lockup({ className }: { className?: string }) {
  return (
    <span className={cn("flex items-center gap-2.5", className)}>
      <Mark className="size-6" />
      {/* The lockup's baseline is the name's (where a row aligns it on baselines, as the footer's). */}
      <span className="self-baseline text-wordmark font-semibold text-foreground">Phala Pay</span>
    </span>
  );
}

function Mark({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 32 32" aria-hidden="true" className={cn("shrink-0", className)}>
      <rect width="32" height="32" rx="8" className="fill-neutral-950" />
      <path
        fillRule="evenodd"
        d="M8 0H24A8 8 0 0 1 32 8V24A8 8 0 0 1 24 32H8A8 8 0 0 1 0 24V8A8 8 0 0 1 8 0ZM8 1.33A6.67 6.67 0 0 0 1.33 8V24A6.67 6.67 0 0 0 8 30.67H24A6.67 6.67 0 0 0 30.67 24V8A6.67 6.67 0 0 0 24 1.33Z"
        className="hidden fill-white/15 dark:block"
      />
      <rect x="10" y="10" width="12" height="12" rx="3" className="fill-brand" />
    </svg>
  );
}

/** The arrowed primary action: deploying a testnet instance. */
function DeployButton({ variant = "primary" }: { variant?: "primary" | "brand" }) {
  return (
    <Button asChild size="lg" variant={variant} className="group">
      <a href={LINKS.deploy}>Start a testnet instance<ArrowRight {...ICON} className={NUDGE} /></a>
    </Button>
  );
}

/**
 * The release's status, a neutral badge (a status, not the brand's accent) in the hero and the
 * footer, linked to the security policy that says what pre-1.0 covers.
 */
export function ReleaseBadge({ className }: { className?: string }) {
  const [stage, ...status] = HERO_META.split(" · ");
  return (
    <a href={LINKS.security} className={cn("group inline-flex min-h-8 items-center gap-2.5 rounded-full border bg-card py-0.5 pr-3 pl-1 text-sm text-body-foreground shadow-card transition-colors hover:text-foreground", className)}>
      <span className="inline-flex h-6 items-center rounded-full bg-muted px-2.5 mono-label text-foreground">{stage}</span>
      <span className="sr-only"> · </span>
      {status.join(" · ")}
      <span className="sr-only">: the security policy</span>
      <ChevronRight {...ICON} className={cn("size-4 text-muted-foreground", NUDGE)} />
    </a>
  );
}

// The headline, the way to run it (self-hosting on Phala Cloud), three facts, and beside them what
// integrating it takes. The release's status leads.
export function Hero({ code }: { code: ReactNode }) {
  return (
    // The two columns centred on each other: the pitch, and the code window with its caption inside.
    <section aria-labelledby="hero-title" className="border-b">
      <div data-layout="split" data-align="center" className={cn(CONTAINER, GRID, "gap-y-14 pt-14 pb-16 sm:pt-20 lg:items-center lg:py-24")}>
        <div data-column="left" className={LEFT}>
          <ReleaseBadge />
          {/* On a phone a step smaller, so the claim's second line ("without a custodian") stays whole. */}
          <h1 id="hero-title" className="mt-7 max-w-xl text-display-xs font-semibold text-balance sm:text-display lg:text-display-sm xl:text-display">
            {TAGLINE}
          </h1>
          <p className="mt-6 max-w-xl text-lead-lg text-pretty text-body-foreground">{unbroken(HERO_SUBHEAD)}</p>
          <div className="mt-10 flex flex-col gap-3 sm:flex-row">
            <DeployButton />
            <Button asChild size="lg" variant="secondary"><a href={LINKS.docs}>Read the docs</a></Button>
          </div>
          {/* Three facts under a rule, each value over its meaning, and the note that qualifies the
              first under them; on a phone, a row each, the values in one column and the meanings in
              the next. */}
          <div className="mt-12 max-w-xl border-t pt-6">
            <dl data-facts className="grid gap-y-3 sm:grid-cols-3 sm:divide-x">
              {HERO_FACTS.map(({ value, label }) => (
                <div key={label} className="grid grid-cols-[7.5rem_minmax(0,1fr)] items-baseline gap-x-4 sm:grid-cols-1 sm:gap-y-1 sm:px-5 sm:first:pl-0">
                  <dt className="text-sm text-pretty text-muted-foreground">{label}</dt>
                  <dd className="order-first text-xl font-semibold tracking-tight whitespace-nowrap tabular-nums sm:text-2xl">{value}</dd>
                </div>
              ))}
            </dl>
            <p data-facts-note className="mt-4 text-sm text-pretty text-muted-foreground">{unbroken(HERO_FACTS_NOTE)}</p>
          </div>
        </div>
        <div data-column="right" className={RIGHT}>{code}</div>
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
        footer={
          // The outcome the code leads to, as the webhook it waits for: the brand's dot is the money.
          // Its dot is centred in a box one line tall, so it sits on the first line however the text wraps.
          <figcaption className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-2.5">
            <span aria-hidden="true" className="flex h-[1lh] items-center"><span className="size-2 rounded-full bg-brand" /></span>
            <span>{HERO_CODE_NOTE.before}<code className="font-mono text-mono text-code-foreground">{HERO_CODE_NOTE.code}</code>{HERO_CODE_NOTE.after}</span>
          </figcaption>
        }
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
            <span className="ml-auto font-mono text-mono text-code-muted">{current?.file}</span>
            {hydrated && current !== undefined
              ? <CopyButton value={current.code} label={`Copy ${current.file}`} className="text-code-muted hover:bg-white/10 hover:text-code-foreground" />
              : <span aria-hidden="true" className="size-8" />}
          </>
        }
      >
        {/* The panels share one grid cell, so the window is as tall as the longer snippet whichever
            is shown; the other is invisible (and so out of the accessibility tree too). */}
        <div className="grid">
          {HERO_CODE.map(({ id: key, file, lines }, index) => (
            <div key={key} role="tabpanel" id={`${id}-panel-${key}`} aria-labelledby={`${id}-tab-${key}`}
              className={cn("col-start-1 row-start-1 min-w-0", index !== shown && "invisible")}>
              {/* On a phone, 12px code in a 16px margin: the snippets' 45-character lines fit 390px. */}
              <CodeBody label={file} className="px-4 text-xs/relaxed sm:px-5 sm:text-mono">
                <HighlightedLines lines={lines} />
              </CodeBody>
            </div>
          ))}
        </div>
      </CodeWindow>
    </figure>
  );
}

/**
 * The demo, directly below the hero, on a band of its own: the product itself, sized so that its
 * heading and both panels fit one 1440×900 screen. Its heading and introduction sit on one
 * baseline, over the two panels' columns. The one section without an eyebrow: its live status, the
 * pill beside the heading, labels it, and an eyebrow's line would take the screen's last 30px
 * (specs/site-v2/design.md).
 */
export function DemoSection({ children }: { children?: ReactNode }) {
  return (
    <section id="demo" aria-labelledby="demo-title" className="scroll-mt-16 border-b bg-surface">
      <div className={cn(CONTAINER, "py-6")}>
        <div data-layout="split" className={cn(GRID, "gap-y-3 lg:items-baseline")}>
          <div data-column="left" className={cn(LEFT, "flex flex-wrap items-baseline gap-x-4 gap-y-2")}>
            <h2 id="demo-title" className={H2}>{DEMO_TITLE}</h2>
            <p className="inline-flex h-7 items-center gap-2 self-center rounded-full border bg-card px-3 text-sm font-medium text-body-foreground shadow-card">
              <span aria-hidden="true" className="relative flex size-2">
                <span className="absolute inline-flex size-full rounded-full bg-brand opacity-75 motion-safe:animate-ping" />
                <span className="relative inline-flex size-2 rounded-full bg-brand ring-1 ring-foreground/25" />
              </span>
              {DEMO_STATUS}
            </p>
          </div>
          <p data-column="right" className={cn(RIGHT, LEAD)}>{DEMO_LEAD}</p>
        </div>
        <div id="demo-root" className="mt-4">{children ?? <DemoPlaceholder />}</div>
      </div>
    </section>
  );
}

/**
 * The demo's place in the static HTML and while the page hydrates. Nothing is reserved: the demo
 * arrives under the hero, so it moves nothing in view (e2e/demo.spec.ts measures the layout shift).
 */
export function DemoPlaceholder() {
  return (
    <noscript>
      <p className="text-sm text-muted-foreground">The demo needs JavaScript.</p>
    </noscript>
  );
}

/** The demo's place while its chunk loads. */
export function DemoLoading() {
  return <p role="status" className="text-sm text-muted-foreground">Loading the demo…</p>;
}

const STATION_ICONS: Record<Station, LucideIcon> = { wallet: Wallet, contract: FileCode2, treasury: Landmark };

/**
 * The path a payment takes, as the page's one diagram: three stations, each joined to the next by a
 * rail in the brand's colour with what passes along it; in a row from xl (narrower, the stations'
 * names would wrap), a column below it. A segment runs along each rail, the way the money goes,
 * where motion is welcome. The treasury, the merchant's own, is the destination: marked in the
 * brand's colour.
 */
function CustodyPath() {
  return (
    <figure>
      <ol className="flex flex-col xl:grid xl:grid-cols-[minmax(0,1fr)_9rem_minmax(0,1fr)_9rem_minmax(0,1fr)]">
        {CUSTODY_PATH.map(({ station, role, name, detail }, index) => {
          const Icon = STATION_ICONS[station];
          const last = station === "treasury";
          return (
            <li key={name} className="contents">
              {index > 0 && (
                <span className="flex items-center gap-3 py-2 pl-8 xl:flex-col xl:justify-center xl:gap-2 xl:px-3 xl:py-0">
                  <MoveDown {...ICON} className="size-5 text-brand-ink xl:hidden" />
                  <span className="font-mono text-sm text-muted-foreground">{CUSTODY_LINKS[index - 1]}</span>
                  <span aria-hidden="true" className="hidden w-full items-center text-brand-ink xl:flex">
                    <span className="relative h-px flex-1 overflow-hidden bg-current/40">
                      <span data-rail-segment className="absolute inset-y-0 w-1/3 bg-current motion-safe:animate-rail" />
                    </span>
                    <svg viewBox="0 0 8 10" className="h-2.5 w-2 fill-current"><path d="M0 0L8 5L0 10Z" /></svg>
                  </span>
                </span>
              )}
              <div className={cn("flex flex-col rounded-xl border bg-card p-6 shadow-card", last && "border-brand-ink/70 ring-1 ring-brand-ink/30")}>
                <div className="flex items-center justify-between gap-4">
                  <span className={cn("flex size-10 items-center justify-center rounded-lg border bg-muted", last && "border-transparent bg-brand text-brand-foreground")}>
                    <Icon {...ICON} className="size-5" />
                  </span>
                  <span className="mono-label text-muted-foreground">0{index + 1} · {role}</span>
                </div>
                <p className="mt-8 text-heading font-semibold">{name}</p>
                <p className="mt-2 text-sm/6 text-pretty text-body-foreground">{detail}</p>
              </div>
            </li>
          );
        })}
      </ol>
      <figcaption className="mx-auto mt-10 flex max-w-2xl items-start justify-center gap-2 text-sm text-pretty text-body-foreground xl:items-center">
        <ShieldCheck {...ICON} className="mt-0.5 size-4 shrink-0 text-brand-ink xl:mt-0" />
        <span>{CUSTODY_NOTE}</span>
      </figcaption>
    </figure>
  );
}

/** Where the money goes, told once, on the band: the claim, centred, and the path under it. */
export function WhereTheMoneyGoes() {
  const [custody] = PROPERTIES;
  return (
    <section aria-labelledby="money-title" className={BAND}>
      <div className={cn(CONTAINER, SECTION)}>
        <Intro id="money-title" align="center" eyebrow={EYEBROWS.custody} title={MONEY_TITLE}
          lead={custody === undefined ? undefined : unbroken(custody.text)} />
        <div className="mt-14 lg:mt-16"><CustodyPath /></div>
      </div>
    </section>
  );
}

/**
 * A feature: what it shows (a figure, a meter, a list) in the card's top half, over a rule, and
 * its title and line under it. From lg the card takes two of the grid's rows as a subgrid, so the
 * cards side by side share their rules and their titles' lines.
 */
function FeatureCard({ title, text, className, children }: { title: string; text: string; className?: string; children: ReactNode }) {
  return (
    <div data-feature className={cn("flex min-w-0 flex-col rounded-xl border bg-card shadow-card lg:row-span-2 lg:grid lg:grid-rows-subgrid lg:gap-y-0", className)}>
      <div data-feature-well className="flex flex-col justify-center border-b px-6 py-7 sm:px-8">{children}</div>
      <div className="px-6 py-7 sm:px-8">
        <h3 className="text-heading font-semibold">{title}</h3>
        <p className="mt-2 text-pretty text-body-foreground">{unbroken(text)}</p>
      </div>
    </div>
  );
}

/** A small label inside a card. */
const CARD_LABEL = "mono-label text-muted-foreground";

/**
 * The time to credit as a meter: one bar per chain on one scale (the slowest is the whole track),
 * one colour, each value printed at its end. Each bar is an SVG rect whose width comes from its
 * seconds: an attribute, not a style (the CSP).
 */
function CreditTimes() {
  const longest = Math.max(...CREDIT_TIMES.rows.map(({ seconds }) => seconds));
  return (
    <figure>
      <figcaption className={cn(CARD_LABEL, "text-pretty")}>{CREDIT_TIMES.caption}</figcaption>
      <dl className="mt-5 grid grid-cols-[5.5rem_minmax(0,1fr)_3.5rem] items-center gap-x-4 gap-y-3.5">
        {CREDIT_TIMES.rows.map(({ chain, seconds, value }) => (
          <div key={chain} className="contents">
            <dt className="text-sm font-medium">{chain}</dt>
            <dd aria-hidden="true">
              <svg className="block h-2 w-full">
                <rect width="100%" height="100%" rx="4" className="fill-muted" />
                <rect width={`${(seconds / longest) * 100}%`} height="100%" rx="4" className="fill-brand-ink" />
              </svg>
            </dd>
            <dd className="text-right font-mono text-sm tabular-nums">{value}</dd>
          </div>
        ))}
      </dl>
      <p className="mt-4 text-sm text-pretty text-muted-foreground">{unbroken(CREDIT_TIMES.note)}</p>
    </figure>
  );
}

/** Names as chips, two to a row from sm (none left alone on a row: each list has four), one below. */
function Chips({ label, items }: { label: string; items: string[] }) {
  return (
    <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
      <p className={cn(CARD_LABEL, "sm:w-24 sm:shrink-0")}>{label}</p>
      <ul className="grid gap-2 sm:grid-cols-2">
        {items.map((item) => (
          <li key={item} className="inline-flex h-7 items-center justify-self-start rounded-md border bg-muted/50 px-2 font-mono text-mono">{item}</li>
        ))}
      </ul>
    </div>
  );
}

/** What it is for, and four facts as a grid of cards: wide and narrow, then narrow and wide. */
export function Features() {
  const [, fee, speed, verify, api] = PROPERTIES;
  if (fee === undefined || speed === undefined || verify === undefined || api === undefined) return null;
  return (
    <section aria-labelledby="features-title" className="border-b">
      <div className={cn(CONTAINER, SECTION)}>
        <Intro id="features-title" eyebrow={EYEBROWS.features} title={FEATURES_TITLE} lead={PROPERTIES_LEAD} />
        <div className="mt-12 grid gap-4 lg:mt-14 lg:grid-cols-12 lg:gap-6">
          {/* Two rows of two cards, wide and narrow, then narrow and wide; each card two of the
              grid's rows (its figure, its text), shared with the card beside it. */}
          <FeatureCard className="lg:col-span-5" title={fee.title} text={fee.text}>
            <p className="text-display-sm font-semibold tabular-nums sm:text-display">{FEE_FIGURE.value}</p>
            <p className={cn(CARD_LABEL, "mt-2")}>{FEE_FIGURE.label}</p>
          </FeatureCard>
          <FeatureCard className="lg:col-span-7" title={speed.title} text={speed.text}><CreditTimes /></FeatureCard>
          <FeatureCard className="lg:col-span-7" title={api.title} text={api.text}>
            <div className="flex flex-col gap-4">
              <Chips label="SDKs" items={SDK_PACKAGES} />
              <Chips label="Webhooks" items={WEBHOOK_EVENTS} />
            </div>
          </FeatureCard>
          <FeatureCard className="lg:col-span-5" title={verify.title} text={verify.text}>
            <ul className="flex flex-col gap-3 text-sm">
              {VERIFY_CHECKS.map((item) => (
                <li key={item} className="flex items-center gap-2.5">
                  <span aria-hidden="true" className="flex size-5 shrink-0 items-center justify-center rounded-full bg-brand text-brand-foreground">
                    <Check strokeWidth={2.5} className="size-3" />
                  </span>
                  {item}
                </li>
              ))}
            </ul>
          </FeatureCard>
        </div>
      </div>
    </section>
  );
}

/** Names in prose: "A, B, and C". */
const list = new Intl.ListFormat("en", { type: "conjunction" });

/**
 * Phala Pay's column in a comparison table: a rule in the brand's colour above its heading and a
 * faint lime tint down it, a highlight rather than a fill, in either theme. Its text stays in the
 * foreground colour.
 */
export const PHALA_COLUMN = "border-t-2 border-t-brand-ink bg-brand/10 text-foreground";
/** A cell of Phala Pay's column. */
export const PHALA_CELL = "bg-brand/10 text-foreground";
/** A comparison table in its frame: the window's radius, the card's edge and shadow. */
export const TABLE_FRAME = "overflow-hidden rounded-xl border bg-card shadow-card";

/**
 * The comparison's summary: from lg a framed table, Phala Pay's column set apart; below lg, Phala Pay
 * beside one provider at a time.
 */
export function CompareTeaser() {
  const [phala, ...others] = TEASER.vendors;
  const more = (
    <Button asChild variant="secondary" className="group">
      <a href="/compare">See the full comparison<ArrowRight {...ICON} className={NUDGE} /></a>
    </Button>
  );
  return (
    // On the surface band: between the features and the questions (both on the page), never beside
    // the lime band, so the two bands meet only once (the demo and the custody path).
    <section aria-labelledby="compare-title" className="border-b bg-surface">
      <div className={cn(CONTAINER, SECTION)}>
        <div className="flex flex-wrap items-end justify-between gap-x-8 gap-y-6">
          <Intro id="compare-title" eyebrow={EYEBROWS.compare} title="How Phala Pay compares"
            lead={<>Beside {list.format(others.map(({ name }) => name))}, as each states it. The full comparison adds {list.format(TEASER_OTHERS)}, with a source for every value.</>} />
          <div className="hidden lg:block">{more}</div>
        </div>
        <div className={cn(TABLE_FRAME, "mt-12 hidden lg:block")}>
          {/* Four equal columns, on the grid's quarters: the dimension, then each provider. */}
          <table className="w-full table-fixed border-collapse text-left">
            <caption className="sr-only">{list.format(TEASER.vendors.map(({ name }) => name))} on {TEASER.dimensions.length} dimensions.</caption>
            <colgroup>
              <col className="w-1/4" />
              {TEASER.vendors.map(({ id }) => <col key={id} className="w-1/4" />)}
            </colgroup>
            <thead>
              <tr className="border-b bg-surface">
                <td />
                {TEASER.vendors.map(({ id, name }) => (
                  <th key={id} scope="col" className={cn("px-6 py-4 align-bottom text-base font-semibold", id === phala?.id ? PHALA_COLUMN : "text-body-foreground")}>
                    {name}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {TEASER.dimensions.map(({ key, label }) => (
                <tr key={key} className="border-b last:border-b-0">
                  <th scope="row" className="px-6 py-5 align-top text-sm/7 font-medium">{label}</th>
                  {TEASER.vendors.map((vendor) => (
                    <td key={vendor.id} className={cn("px-6 py-5 align-top text-base/7 text-pretty", vendor.id === phala?.id ? PHALA_CELL : "text-body-foreground")}>
                      <ComparisonCell cell={vendor[key]} linkSource={false} />
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        {phala !== undefined && (
          <Versus phala={phala} others={others} dimensions={TEASER.dimensions} linkSource={false} name="teaser-versus" className="mt-10 lg:hidden" />
        )}
        <div className="mt-8 lg:hidden">{more}</div>
      </div>
    </section>
  );
}

/**
 * The questions, each answer folded under it natively (no script, so it works before and without
 * hydration), in two thirds of the grid; their heading in the first third, its first line on the
 * first question's baseline, staying in view beside them from lg.
 */
export function Faq() {
  return (
    <section aria-labelledby="faq-title" className="border-b">
      <div className={cn(CONTAINER, SECTION)}>
        <Eyebrow>{EYEBROWS.faq}</Eyebrow>
        <div data-layout="aside" className={cn(GRID, "mt-4 gap-y-10 lg:items-baseline")}>
          <div data-column="left" className={cn(ASIDE, "lg:sticky lg:top-24")}>
            <h2 id="faq-title" className={H2}>Frequently asked questions</h2>
            <p className={cn(LEAD, "mt-5")}>
              Not answered here? Read the <a className={TEXT_LINK} href={LINKS.docs}>documentation</a> or ask on <a className={TEXT_LINK} href={LINKS.issues}>GitHub</a>.
            </p>
          </div>
          <div data-column="right" className={cn(MAIN, "border-t")}>
            {FAQ.map(({ question, answer }) => (
              <details key={question} className="group border-b">
                <summary className="flex min-h-16 cursor-pointer list-none items-center justify-between gap-6 py-5 text-base font-medium sm:text-lg [&::-webkit-details-marker]:hidden">
                  <span>{question}</span>
                  <span aria-hidden="true" className="flex size-8 shrink-0 items-center justify-center rounded-full border bg-card text-muted-foreground shadow-card">
                    <Plus strokeWidth={1.75} className="size-4 group-open:rotate-45 motion-safe:transition-transform" />
                  </span>
                </summary>
                <p className="max-w-2xl pb-7 leading-7 text-pretty text-body-foreground sm:pr-14">{unbroken(answer)}</p>
              </details>
            ))}
          </div>
        </div>
      </div>
    </section>
  );
}

/**
 * The close, on the band: the mark, the call to deploy in the brand's colour (its one lime button,
 * dark text on the fill), and under it the one command that does.
 */
export function ClosingCta({ command }: { command: ReactNode }) {
  return (
    <section aria-labelledby="closing-title" className={BAND}>
      <div className={cn(CONTAINER, "py-24 lg:py-32")}>
        <div className="mx-auto flex max-w-2xl flex-col items-center text-center">
          <Mark className="size-12" />
          <h2 id="closing-title" className={cn(H2, "mt-8")}>{CLOSING_TITLE}</h2>
          <p className={cn(LEAD, "mt-5")}>{CLOSING_LEAD}</p>
          <div className="mt-10 flex w-full flex-col justify-center gap-3 sm:w-auto sm:flex-row">
            <DeployButton variant="brand" />
            <Button asChild size="lg" variant="secondary"><a href={LINKS.repo}>View on GitHub</a></Button>
          </div>
        </div>
        <div className="mx-auto mt-14 max-w-2xl">{command}</div>
      </div>
    </section>
  );
}

/**
 * The one-command deploy in a terminal window, an island for its copy button. The command wraps
 * rather than scrolling: every character stays in view on a phone.
 */
export function DeployCommand() {
  const hydrated = useHydrated();
  return (
    <CodeWindow
      // Two lines of a length, so the link never sits alone on a short last line.
      footer={<p className="text-balance">
        Deploys the latest release. To verify the release's provenance first, follow
        the <a className={cn(TEXT_LINK, "whitespace-nowrap text-code-foreground decoration-code-foreground/40 hover:decoration-code-foreground")} href={LINKS.deploy}>high-assurance path</a>.
      </p>}
      header={<>
      <span className="text-sm text-code-muted">Terminal</span>
      {hydrated
        ? <CopyButton value={DEPLOY_COMMAND} label="Copy the deploy command" className="ml-auto text-code-muted hover:bg-white/10 hover:text-code-foreground" />
        : <span aria-hidden="true" className="ml-auto size-8" />}
    </>}>
      <pre className="px-5 py-4 font-mono text-mono whitespace-pre-wrap text-code-foreground [overflow-wrap:anywhere]">
        <code><span aria-hidden="true" className="text-code-muted select-none">$ </span>{DEPLOY_COMMAND}</code>
      </pre>
    </CodeWindow>
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
      { href: LINKS.changelog, label: "Changelog" },
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

/**
 * The brand in the first third (the name, the line, the release's status), the links in the other
 * two: the brand's name and the columns' titles on one baseline.
 */
export function SiteFooter() {
  return (
    <footer className="border-t">
      <div data-layout="aside" className={cn(CONTAINER, GRID, "gap-y-12 pt-16 pb-10 text-sm lg:items-baseline")}>
        <div data-column="left" className={ASIDE}>
          <Lockup />
          <p className="mt-4 max-w-xs text-pretty text-body-foreground">{TAGLINE}. Open source, self-hosted, on Ethereum and Base.</p>
          <ReleaseBadge className="mt-6" />
        </div>
        {/* Four columns of links where they fit (from sm), two by two on a phone. */}
        <div data-column="right" className={cn(MAIN, "grid grid-cols-2 gap-x-8 gap-y-10 sm:grid-cols-4")}>
          {FOOTER.map((column) => (
            <nav key={column.title} aria-label={column.title}>
              <p className="mono-label text-muted-foreground">{column.title}</p>
              {/* Rows 44px tall for touch, 32px from md. */}
              <ul className="mt-3">
                {column.links.map(({ href, label }) => (
                  <li key={label}>
                    <a className="inline-flex min-h-11 items-center text-body-foreground transition-colors hover:text-foreground md:min-h-8" href={href}>
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
