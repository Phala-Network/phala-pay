import { cn } from "@/lib/utils";
import { ComparisonCell } from "./ComparisonCell.js";
import { COMPARE_ACCESSED, competitors, dimensions, phalaPay, sources } from "./content/compare.js";
import { CONTAINER, H2, LEAD, LINKS, PHALA_COLUMN, TEXT_LINK } from "./Site.js";
import { Versus } from "./Versus.js";

const vendors = [phalaPay, ...competitors];
const SECTION = "scroll-mt-20 border-t pt-12 lg:pt-16";

/**
 * From md, a table whose first column stays put while the rest scrolls; it needs 64rem, so from
 * 1024px of container all six vendors show, and below that the right edge fades and a line says it
 * scrolls. Phala Pay's column is marked by a rule above it. Below md, one provider at a time.
 */
function ComparisonTable() {
  return (
    <div className="@container mt-8 hidden md:block">
      <p aria-hidden="true" className="mb-3 hidden text-sm text-muted-foreground @max-5xl:block">Scroll for all six →</p>
      <div role="region" aria-label="Comparison table" tabIndex={0}
        className="overflow-x-auto rounded-sm @max-5xl:pr-16 @max-5xl:mask-r-from-[calc(100%-4rem)]">
        <table className="w-full min-w-5xl table-fixed border-separate border-spacing-0 text-left text-sm">
          <caption className="sr-only">Phala Pay and five crypto payment services, compared across ten dimensions.</caption>
          <thead>
            <tr>
              <th scope="col" className="sticky left-0 z-10 w-40 border-r bg-background pr-4 pb-4 align-bottom text-xs font-medium text-muted-foreground">
                Dimension
              </th>
              {vendors.map(({ id, name }) => (
                <th key={id} scope="col"
                  className={cn("border-t-2 px-4 pt-4 pb-4 align-bottom text-table font-semibold", id === phalaPay.id ? PHALA_COLUMN : "border-transparent text-body-foreground")}>
                  {name}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {dimensions.map(({ key, label }) => (
              <tr key={key}>
                <th scope="row" className="sticky left-0 z-10 border-t border-r bg-background py-4 pr-4 align-top font-medium">
                  {label}
                </th>
                {vendors.map((vendor) => (
                  <td key={vendor.id} className={cn("border-t px-4 py-4 align-top leading-6 text-pretty", vendor.id === phalaPay.id ? "bg-muted/50 text-foreground" : "text-body-foreground")}>
                    <ComparisonCell cell={vendor[key]} linkSource />
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
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
      {/* The lead's last line sits on the title's baseline. */}
      <div className="grid gap-6 lg:grid-cols-12 lg:items-baseline-last lg:gap-10">
        <div className="lg:col-span-7">
          <p className="text-sm font-medium text-muted-foreground">Compare</p>
          <h1 className="mt-3 text-display-sm font-semibold text-balance sm:text-display">How Phala Pay compares</h1>
        </div>
        <div className="lg:col-span-5">
          <p className={LEAD}>Custody, fees, chains, speed, and refunds across six ways to accept crypto, each as its vendor states it, with a source for every value.</p>
          <p className="mt-3 text-sm text-muted-foreground">
            Last checked {COMPARE_ACCESSED}. Competitor terms change; check their sites before deciding.
          </p>
        </div>
      </div>
      <nav aria-label="On this page" className="mt-10 border-y">
        <ul className="flex flex-wrap gap-x-8 text-sm">
          {CONTENTS.map(({ href, label }) => (
            <li key={href}>
              <a href={href} className="inline-flex min-h-12 items-center font-medium text-muted-foreground transition-colors hover:text-foreground">
                {label}
              </a>
            </li>
          ))}
        </ul>
      </nav>

      <section id="glance" aria-labelledby="glance-title" className="mt-12 scroll-mt-20 lg:mt-16">
        <h2 id="glance-title" className={H2}>At a glance</h2>
        <ComparisonTable />
        <Versus phala={phalaPay} others={competitors} dimensions={dimensions} linkSource name="compare-versus" className="mt-8 md:hidden" />
        <div className="mt-6 grid gap-1 text-sm text-muted-foreground">
          <p id="partial-note">(partial): Partially stated by the vendor; see source.</p>
          <p id="not-stated-note">—: Not stated publicly as of {COMPARE_ACCESSED}.</p>
        </div>
      </section>

      <section id="fit" aria-labelledby="fit-title" className={cn(SECTION, "mt-16 lg:mt-24")}>
        <h2 id="fit-title" className={H2}>Where each fits</h2>
        <div className="mt-10 grid gap-10 lg:grid-cols-3 lg:gap-10">
          {FIT.map(({ title, text }, index) => (
            <div key={title} className={cn("border-t-2 pt-5", index === FIT.length - 1 ? "border-foreground" : "border-border")}>
              <h3 className="text-heading font-semibold">{title}</h3>
              <p className="mt-2 text-pretty text-body-foreground">{text}</p>
              {index === FIT.length - 1 && (
                <a href={LINKS.deploy} className={cn(TEXT_LINK, "mt-3 inline-flex min-h-11 items-center text-sm")}>Start a testnet instance</a>
              )}
            </div>
          ))}
        </div>
      </section>

      <section id="sources" aria-labelledby="sources-title" className={cn(SECTION, "mt-16 lg:mt-24")}>
        <div className="grid gap-6 lg:grid-cols-12 lg:gap-10">
          <div className="lg:col-span-4">
            <h2 id="sources-title" className={H2}>Sources</h2>
            <p className="mt-3 text-sm text-muted-foreground">All accessed {COMPARE_ACCESSED}.</p>
          </div>
          <ol className="list-decimal gap-10 pl-6 text-sm/6 marker:text-muted-foreground marker:tabular-nums md:columns-2 lg:col-span-8">
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
