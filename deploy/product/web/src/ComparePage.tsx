import { cn } from "@/lib/utils";
import { ComparisonCell } from "./ComparisonCell.js";
import { COMPARE_ACCESSED, competitors, dimensions, phalaPay, sources } from "./content/compare.js";
import { ASIDE, GRID, LEFT, MAIN, RIGHT } from "./layout.js";
import { unbroken } from "./text.js";
import { CONTAINER, Eyebrow, H2, LEAD, LINKS, PHALA_CELL, PHALA_COLUMN, TABLE_FRAME, TEXT_LINK } from "./Site.js";
import { Versus } from "./Versus.js";

const vendors = [phalaPay, ...competitors];
const SECTION = "scroll-mt-20 border-t pt-12 lg:pt-16";

/**
 * From xl, the whole table in its frame: six vendors, ten dimensions, no scrolling. Phala Pay's
 * column is marked by a rule above it and a tint. Below xl, where six columns would crowd, one
 * provider at a time beside Phala Pay (Versus).
 */
function ComparisonTable() {
  return (
    <div className={cn(TABLE_FRAME, "mt-10 hidden xl:block")}>
      <table className="w-full table-fixed border-collapse text-left text-sm">
        <caption className="sr-only">Phala Pay and five crypto payment services, compared across ten dimensions.</caption>
        <thead>
          <tr className="border-b bg-surface">
            <th scope="col" className="w-44 py-4 pr-4 pl-5 align-bottom mono-label text-muted-foreground">
              Dimension
            </th>
            {vendors.map(({ id, name }) => (
              <th key={id} scope="col"
                className={cn("px-4 pt-4 pb-4 align-bottom text-table font-semibold", id === phalaPay.id ? PHALA_COLUMN : "text-body-foreground")}>
                {name}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {dimensions.map(({ key, label }) => (
            <tr key={key} className="border-b last:border-b-0">
              <th scope="row" className="py-4 pr-4 pl-5 align-top font-medium">
                {label}
              </th>
              {vendors.map((vendor) => (
                <td key={vendor.id} className={cn("px-4 py-4 align-top leading-6 text-pretty wrap-break-word", vendor.id === phalaPay.id ? PHALA_CELL : "text-body-foreground")}>
                  <ComparisonCell cell={vendor[key]} linkSource />
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

const CONTENTS = [
  { href: "#glance", label: "At a glance" },
  { href: "#fit", label: "Where each fits" },
  { href: "#sources", label: "Sources" },
];

const FIT = [
  { title: "Hosted processors fit better", text: "Nothing to run; a fee per payment; they onboard you. They operate the payment infrastructure for you; settlement and payout options vary by provider." },
  { title: "BTCPay Server fits better", text: "Self-hosted and free; built for Bitcoin. Bitcoin on-chain and Lightning, with community altcoins via plugins." },
  { title: "Phala Pay fits", text: "Self-hosted and free; built for ERC-20 tokens on Ethereum and Base, with verifiable attestation. You run your own Phala Cloud instance, RPC providers, and backups. There is no fiat settlement or dashboard; you own compliance." },
];

export function ComparePage() {
  return (
    <main id="top" className={`${CONTAINER} flex-1 pt-14 pb-20 sm:pt-20 lg:pb-28`}>
      <Eyebrow>Compare</Eyebrow>
      {/* The split header (src/layout.ts): the title in the left half, the introduction in the
          right, their first lines on one baseline. */}
      <div data-layout="split" className={cn(GRID, "mt-4 gap-y-4 lg:items-baseline")}>
        <h1 data-column="left" className={cn(H2, LEFT)}>{unbroken("How Phala Pay compares")}</h1>
        <div data-column="right" className={RIGHT}>
          <p className={LEAD}>Custody, fees, chains, speed, and refunds across six ways to accept crypto, each as its vendor states it, with a source for every value.</p>
          <p className="mt-3 text-sm text-muted-foreground">
            Last checked {COMPARE_ACCESSED}. Competitor terms change; check their sites before deciding.
          </p>
        </div>
      </div>
      <nav aria-label="On this page" className="mt-12 border-y">
        <ul className="flex flex-wrap gap-x-8 text-sm">
          {CONTENTS.map(({ href, label }, index) => (
            <li key={href}>
              <a href={href} className="inline-flex min-h-12 items-center gap-2.5 font-medium text-muted-foreground transition-colors hover:text-foreground">
                <span aria-hidden="true" className="mono-label text-muted-foreground">0{index + 1}</span>
                {label}
              </a>
            </li>
          ))}
        </ul>
      </nav>

      <section id="glance" aria-labelledby="glance-title" className="mt-12 scroll-mt-20 lg:mt-16">
        <h2 id="glance-title" className={H2}>At a glance</h2>
        <ComparisonTable />
        <Versus phala={phalaPay} others={competitors} dimensions={dimensions} linkSource name="compare-versus" className="mt-8 xl:hidden" />
        <div className="mt-6 grid gap-1 text-sm text-muted-foreground">
          <p id="partial-note">(partial): Partially stated by the vendor; see source.</p>
          <p id="not-stated-note">—: Not stated publicly as of {COMPARE_ACCESSED}.</p>
        </div>
      </section>

      <section id="fit" aria-labelledby="fit-title" className={cn(SECTION, "mt-16 lg:mt-24")}>
        <h2 id="fit-title" className={H2}>Where each fits</h2>
        {/* The two alternatives side by side, their texts of a length; under them Phala Pay across
            the grid, its text beside the way to start, marked as the brand's. */}
        <div className="mt-10 grid gap-4 lg:grid-cols-2 lg:gap-6">
          {FIT.map(({ title, text }, index) => {
            const ours = index === FIT.length - 1;
            return (
              <div key={title} className={cn("flex flex-col rounded-xl border bg-card p-6 shadow-card sm:p-8", ours && "border-brand-ink/70 ring-1 ring-brand-ink/30 lg:col-span-2 lg:flex-row lg:items-end lg:justify-between lg:gap-12")}>
                <div className={cn(ours && "max-w-3xl")}>
                  <h3 className="text-heading font-semibold">{unbroken(title)}</h3>
                  <p className="mt-2 text-pretty text-body-foreground">{unbroken(text)}</p>
                </div>
                {ours && (
                  <a href={LINKS.deploy} className={cn(TEXT_LINK, "mt-3 inline-flex min-h-11 shrink-0 items-center self-start text-sm lg:mt-0 lg:self-end")}>Start a testnet instance</a>
                )}
              </div>
            );
          })}
        </div>
      </section>

      <section id="sources" aria-labelledby="sources-title" className={cn(SECTION, "mt-16 lg:mt-24")}>
        {/* The aside (src/layout.ts), as the FAQ's: the heading stays in view beside the list. */}
        <div data-layout="aside" className={cn(GRID, "gap-y-6 lg:items-baseline")}>
          <div data-column="left" className={cn(ASIDE, "lg:sticky lg:top-24")}>
            <h2 id="sources-title" className={H2}>Sources</h2>
            <p className="mt-3 text-sm text-muted-foreground">All accessed {COMPARE_ACCESSED}.</p>
          </div>
          <ol data-column="right" className={cn(MAIN, "list-decimal gap-8 pl-6 text-sm/6 marker:text-muted-foreground marker:tabular-nums md:columns-2")}>
            {sources.map(({ url, title, host, archived }, index) => (
              <li key={url} id={`source-${index + 1}`} className="mb-3 scroll-mt-20 break-inside-avoid pl-1">
                <a href={url} className={TEXT_LINK}>{title}</a>
                <span className="text-muted-foreground"> · {host}{archived && " · read via archive snapshot"}</span>
              </li>
            ))}
          </ol>
        </div>
      </section>
    </main>
  );
}
