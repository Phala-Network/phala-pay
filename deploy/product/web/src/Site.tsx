import { Menu, MoveDown, MoveRight, Plus, X } from "lucide-react";
import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { CopyButton } from "@/components/ui/copy-button";
import { CodeBody, CodeWindow, HighlightedLines } from "@/components/code";
import { cn } from "@/lib/utils";
import { GRID, LEFT, RIGHT } from "./layout.js";
import { useHydrated } from "./islands.js";
import { ComparisonCell } from "./ComparisonCell.js";
import { Versus } from "./Versus.js";
import { TEASER, TEASER_OTHERS } from "./content/compare.js";
import { HERO_CODE, HERO_CODE_NOTE } from "./content/hero-code.js";
import {
  CLOSING_LEAD, CLOSING_TITLE, CUSTODY_LINKS, CUSTODY_NOTE, CUSTODY_PATH, DEMO_LEAD, DEMO_STATUS, DEMO_TITLE, DEPLOY_COMMAND, FAQ,
  HERO_META, HERO_SUBHEAD, MONEY_TITLE, PROPERTIES, PROPERTIES_LEAD, TAGLINE,
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
 * The page's width: one for every section and the header and footer, so all share a left edge
 * (1280px, 16 to 32px gutters); the 12-column grid inside it is src/layout.ts.
 */
export const CONTAINER = "mx-auto w-full max-w-7xl px-4 sm:px-6 lg:px-8";
/** Every H2, and every other page's H1. */
export const H2 = "text-title-sm font-semibold text-balance sm:text-title";
/** A section's introduction under its heading. */
export const LEAD = "text-lead text-pretty text-body-foreground";
/** One rhythm for every section: 64px above and below on phones, 80px from lg; below the 64px header when scrolled to. */
export const SECTION = "scroll-mt-16 py-16 lg:py-20";
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
      {/* The lockup's baseline is the name's (where a row aligns it on baselines, as the footer's). */}
      <span className="self-baseline text-wordmark font-semibold text-foreground">Phala Pay</span>
    </span>
  );
}

// The headline, the way to run it (self-hosting on Phala Cloud), and beside them, what integrating it
// takes. The facts behind it (fees, speed, custody) follow the demo, once each.
export function Hero({ code }: { code: ReactNode }) {
  return (
    // The two columns centred on each other: the pitch, and the code window with its caption inside.
    <section aria-labelledby="hero-title" className="border-b">
      <div data-align="center" className={cn(CONTAINER, GRID, "gap-y-12 pt-14 pb-16 sm:pt-20 lg:items-center lg:py-20")}>
        <div data-column="left" className={LEFT}>
          <h1 id="hero-title" className="max-w-xl text-display-sm font-semibold text-balance sm:text-display lg:text-display-sm xl:text-display">
            {TAGLINE}
          </h1>
          <p className="mt-6 max-w-xl text-lead text-pretty text-body-foreground">{unbroken(HERO_SUBHEAD)}</p>
          <div className="mt-8 flex flex-col gap-3 sm:flex-row">
            <Button asChild size="lg"><a href={LINKS.deploy}>Start a testnet instance</a></Button>
            <Button asChild size="lg" variant="secondary"><a href={LINKS.docs}>Read the docs</a></Button>
          </div>
          <p className="mt-4 text-sm text-muted-foreground">{HERO_META}</p>
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
          <figcaption>
            {HERO_CODE_NOTE.before}<code className="font-mono text-mono text-code-foreground">{HERO_CODE_NOTE.code}</code>{HERO_CODE_NOTE.after}
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
 * Every section's heading and introduction, one way: the heading in the left half, the
 * introduction in the right, their first lines on one baseline; stacked below lg.
 */
function SectionHeader({ id, title, aside, lead }: { id: string; title: string; aside?: ReactNode; lead?: ReactNode }) {
  return (
    <div className={cn(GRID, "gap-y-3 lg:items-baseline")}>
      <div data-column="left" className={cn(LEFT, "flex flex-wrap items-baseline gap-x-4 gap-y-1")}>
        <h2 id={id} className={H2}>{title}</h2>
        {aside}
      </div>
      {lead !== undefined && <p data-column="right" className={cn(LEAD, RIGHT)}>{lead}</p>}
    </div>
  );
}

/**
 * The demo, directly below the hero, on a band of its own: the product itself, sized so that its
 * heading and both panels fit one 1440×900 screen.
 */
export function DemoSection({ children }: { children?: ReactNode }) {
  return (
    <section id="demo" aria-labelledby="demo-title" className="scroll-mt-16 border-b bg-surface py-6">
      <div className={CONTAINER}>
        <SectionHeader
          id="demo-title"
          title={DEMO_TITLE}
          aside={
            <p className="flex items-center gap-2 text-sm font-medium text-muted-foreground">
              <span aria-hidden="true" className="size-2 rounded-full bg-brand ring-1 ring-foreground/25" />
              {DEMO_STATUS}
            </p>
          }
          lead={DEMO_LEAD}
        />
        <div id="demo-root" className="mt-5">{children ?? <DemoPlaceholder />}</div>
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

/**
 * The path a payment takes, drawn as the page's one diagram: three stations in a row from xl (a
 * column below it, where a row would wrap the stations' names), each joined to the next by an
 * arrow under its label, with room around it. The last station, the merchant's own, is set apart.
 */
function CustodyPath() {
  return (
    <figure>
      <ol className="flex flex-col gap-2 xl:flex-row xl:items-stretch">
        {CUSTODY_PATH.map(({ role, name, detail }, index) => (
          <li key={name} className="contents">
            {index > 0 && (
              <span className="flex items-center gap-2 py-1 pl-6 text-sm text-muted-foreground xl:w-36 xl:shrink-0 xl:flex-col xl:justify-center xl:gap-1 xl:px-4 xl:py-0">
                <MoveDown {...ICON} className="size-5 xl:hidden" />
                <span>{CUSTODY_LINKS[index - 1]}</span>
                <MoveRight {...ICON} className="hidden size-6 xl:block" />
              </span>
            )}
            <div className={cn("flex-1 rounded-lg border bg-card p-6", index === CUSTODY_PATH.length - 1 && "border-foreground")}>
              <p className="text-xs font-medium tracking-wider text-muted-foreground uppercase">{role}</p>
              <p className="mt-2 font-semibold">{name}</p>
              <p className="mt-1.5 text-sm/6 text-pretty text-body-foreground">{detail}</p>
            </div>
          </li>
        ))}
      </ol>
      <figcaption className="mt-5 text-sm text-muted-foreground">{CUSTODY_NOTE}</figcaption>
    </figure>
  );
}

/**
 * What the demo just showed, told once: where the money goes, as a diagram under the claim, then the
 * other facts as a ruled spec list (a term and its line) in the right half, its heading in the left.
 */
export function WhereTheMoneyGoes() {
  const [custody, ...rest] = PROPERTIES;
  return (
    <section aria-labelledby="money-title" className={SECTION}>
      <div className={CONTAINER}>
        <SectionHeader id="money-title" title={MONEY_TITLE} lead={custody === undefined ? undefined : unbroken(custody.text)} />
        <div className="mt-10 lg:mt-12">
          <CustodyPath />
        </div>
        {/* The heading's first line on the list's first term's baseline, as every header's is. */}
        <div className={cn(GRID, "mt-16 gap-y-6 lg:items-baseline")}>
          <div data-column="left" className={LEFT}>
            <h3 className="text-heading font-semibold">For platforms that sell credits</h3>
            <p className="mt-2 max-w-sm text-pretty text-body-foreground">{PROPERTIES_LEAD}</p>
          </div>
          <dl data-column="right" className={cn(RIGHT, "border-t")}>
            {rest.map(({ title, text }) => (
              <div key={title} className="grid gap-1.5 border-b py-5 xl:grid-cols-[10rem_minmax(0,1fr)] xl:gap-6">
                <dt className="font-semibold">{title}</dt>
                <dd className="leading-7 text-pretty text-body-foreground">{unbroken(text)}</dd>
              </div>
            ))}
          </dl>
        </div>
      </div>
    </section>
  );
}

/** Names in prose: "A, B, and C". */
const list = new Intl.ListFormat("en", { type: "conjunction" });

/**
 * The comparison's summary: from md a table, Phala Pay's column set apart by a tint and a rule above
 * it; on a phone, Phala Pay beside one provider at a time.
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
        <div className="mt-10 hidden lg:block">
          {/* Four equal columns, on the grid's quarters: the dimension, then each provider. */}
          <table className="w-full table-fixed border-collapse text-left">
            <caption className="sr-only">{list.format(TEASER.vendors.map(({ name }) => name))} on {TEASER.dimensions.length} dimensions.</caption>
            <colgroup>
              <col className="w-1/4" />
              {TEASER.vendors.map(({ id }) => <col key={id} className="w-1/4" />)}
            </colgroup>
            <thead>
              <tr>
                <td />
                {TEASER.vendors.map(({ id, name }) => (
                  <th key={id} scope="col" className={cn("border-t-2 px-6 pt-4 pb-4 align-bottom text-base font-semibold", id === phala?.id ? PHALA_COLUMN : "border-transparent text-body-foreground")}>
                    {name}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {TEASER.dimensions.map(({ key, label }) => (
                <tr key={key} className="border-t">
                  <th scope="row" className="py-6 pr-6 align-top text-sm/7 font-medium text-muted-foreground">{label}</th>
                  {TEASER.vendors.map((vendor) => (
                    <td key={vendor.id} className={cn("px-6 py-6 align-top text-base/7 text-pretty", vendor.id === phala?.id ? PHALA_CELL : "text-body-foreground")}>
                      <ComparisonCell cell={vendor[key]} linkSource={false} />
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
            {/* The table's footer: the way to the full comparison, at its right edge. */}
            <tfoot>
              <tr className="border-t">
                <td colSpan={TEASER.vendors.length + 1} className="pt-4 text-right">
                  <a href="/compare" className={cn(TEXT_LINK, "inline-flex min-h-11 items-center text-sm")}>See the full comparison</a>
                </td>
              </tr>
            </tfoot>
          </table>
        </div>
        {phala !== undefined && (
          <Versus phala={phala} others={others} dimensions={TEASER.dimensions} linkSource={false} name="teaser-versus" className="mt-8 lg:hidden" />
        )}
        <a href="/compare" className={cn(TEXT_LINK, "mt-6 inline-flex min-h-11 items-center text-sm lg:hidden")}>
          See the full comparison
        </a>
      </div>
    </section>
  );
}

/**
 * Phala Pay's column in a comparison table: a rule above its heading and a faint tint down it, a
 * highlight rather than a fill, in either theme.
 */
export const PHALA_COLUMN = "border-foreground bg-foreground/3 text-foreground";
/** A cell of Phala Pay's column. */
export const PHALA_CELL = "bg-foreground/3 text-foreground";

/** Each answer folds under its question, natively: no script, so it works before and without hydration. */
export function Faq() {
  return (
    <section aria-labelledby="faq-title" className={cn(SECTION, "border-t")}>
      {/* The section's header, as every section's; the questions under its introduction. */}
      <div className={CONTAINER}>
        <SectionHeader
          id="faq-title"
          title="Frequently asked questions"
          lead={<>Not answered here? Read the <a className={TEXT_LINK} href={LINKS.docs}>documentation</a> or ask on <a className={TEXT_LINK} href={LINKS.issues}>GitHub</a>.</>}
        />
        <div className={cn(GRID, "mt-8")}>
          <div data-column="right" className={cn(RIGHT, "border-t")}>
            {FAQ.map(({ question, answer }) => (
              <details key={question} className="group border-b">
                <summary className="flex min-h-16 cursor-pointer list-none items-center justify-between gap-6 py-4 text-base font-medium sm:text-lg [&::-webkit-details-marker]:hidden">
                  {question}
                  <Plus {...ICON} className="size-5 shrink-0 text-muted-foreground group-open:rotate-45 motion-safe:transition-transform" />
                </summary>
                <p className="pr-10 pb-6 leading-7 text-pretty text-body-foreground">{unbroken(answer)}</p>
              </details>
            ))}
          </div>
        </div>
      </div>
    </section>
  );
}

/**
 * The close: a band of its own, as the demo's is, with the call to deploy, and beside it the one
 * command that does.
 */
export function ClosingCta({ command }: { command: ReactNode }) {
  return (
    <section aria-labelledby="closing-title" className="border-t bg-surface py-16 lg:py-20">
      <div data-align="center" className={cn(CONTAINER, GRID, "gap-y-10 lg:items-center")}>
        <div data-column="left" className={LEFT}>
          <h2 id="closing-title" className={H2}>{CLOSING_TITLE}</h2>
          <p className={cn(LEAD, "mt-4 max-w-md")}>{CLOSING_LEAD}</p>
          <div className="mt-8 flex flex-col gap-3 sm:flex-row">
            <Button asChild size="lg"><a href={LINKS.deploy}>Start a testnet instance</a></Button>
            <Button asChild size="lg" variant="secondary"><a href={LINKS.repo}>View on GitHub</a></Button>
          </div>
        </div>
        <div data-column="right" className={RIGHT}>{command}</div>
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
      footer={<>
        Deploys the latest release. To verify the release's provenance first, follow
        the <a className={cn(TEXT_LINK, "whitespace-nowrap text-code-foreground decoration-code-foreground/40 hover:decoration-code-foreground")} href={LINKS.deploy}>high-assurance path</a>.
      </>}
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
      <div className={cn(CONTAINER, GRID, "gap-y-12 pt-14 pb-10 text-sm lg:items-baseline lg:pt-16")}>
        <div data-column="left" className={LEFT}>
          <Lockup />
          <p className="mt-4 max-w-xs text-pretty text-muted-foreground">{TAGLINE}. Open source, self-hosted, on Ethereum and Base.</p>
        </div>
        {/* Four columns of links where they fit (from sm, and from xl beside the brand); two by two
            in the right half between lg and xl, where four would break their names. */}
        <div data-column="right" className={cn(RIGHT, "grid grid-cols-2 gap-x-6 gap-y-10 sm:grid-cols-4 lg:grid-cols-2 xl:grid-cols-4")}>
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
