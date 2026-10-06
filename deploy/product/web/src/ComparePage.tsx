import { ComparisonCell } from "./ComparisonCell.js";
import { COMPARE_ACCESSED, competitors, dimensions, phalaPay, sources } from "./content/compare.js";
import { CONTAINER, H2, LINKS } from "./Site.js";

const vendors = [phalaPay, ...competitors];
const SECTION = "mt-16 scroll-mt-20";

/**
 * From md, a table whose first column stays put while the rest scrolls; it needs 64rem, so from
 * 1024px of container all six vendors show, and below that the right edge fades and a line says it
 * scrolls. Below md, one list per dimension.
 */
function ComparisonTable() {
  const highlight = (id: string) => (id === phalaPay.id ? "bg-muted/40" : "");
  return (
    <div className="@container mt-6 hidden md:block">
      <p aria-hidden="true" className="mb-3 hidden text-sm text-muted-foreground @max-5xl:block">Scroll for all six →</p>
      <div role="region" aria-label="Comparison table" tabIndex={0}
        className="overflow-x-auto rounded-sm @max-5xl:pr-16 @max-5xl:mask-r-from-[calc(100%-4rem)]">
        <table className="w-full min-w-5xl table-fixed border-separate border-spacing-0 text-sm">
          <caption className="sr-only">Phala Pay and five crypto payment services, compared across ten dimensions.</caption>
          <thead>
            <tr>
              <th scope="col" className="sticky left-0 z-10 w-44 border-r border-b bg-background px-4 py-3 text-left align-bottom text-xs font-medium text-muted-foreground">
                Dimension
              </th>
              {vendors.map(({ id, name }) => (
                <th key={id} scope="col" className={`border-b px-4 py-3 text-left align-bottom font-medium ${highlight(id)}`}>{name}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {dimensions.map(({ key, label }) => (
              <tr key={key}>
                <th scope="row" className="sticky left-0 z-10 border-r border-b bg-background px-4 py-3 text-left align-top font-medium">
                  {label}
                </th>
                {vendors.map((vendor) => (
                  <td key={vendor.id} className={`border-b px-4 py-3 align-top leading-6 ${highlight(vendor.id)}`}>
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

function ComparisonList() {
  return (
    <div className="mt-6 border-b md:hidden">
      {dimensions.map(({ key, label }) => (
        <div key={key} className="border-t py-6">
          <h3 className="font-semibold">{label}</h3>
          <dl className="mt-3 grid gap-4 text-sm">
            {vendors.map((vendor) => (
              <div key={vendor.id}>
                <dt className="font-medium">{vendor.name}</dt>
                <dd className="mt-1 leading-6 text-muted-foreground"><ComparisonCell cell={vendor[key]} linkSource /></dd>
              </div>
            ))}
          </dl>
        </div>
      ))}
    </div>
  );
}

const CONTENTS = [
  { href: "#glance", label: "At a glance" },
  { href: "#fit", label: "Where each fits" },
  { href: "#sources", label: "Sources" },
];

export function ComparePage() {
  return (
    <main id="top" className={`${CONTAINER} flex-1 pt-12 pb-16 md:pt-20 md:pb-24`}>
      <h1 className="text-3xl/tight font-semibold tracking-tight sm:text-4xl/tight">How Phala Pay compares</h1>
      <p className="mt-4 max-w-prose text-pretty text-muted-foreground">
        Last checked {COMPARE_ACCESSED}. Competitor terms change; check their sites before deciding.
      </p>
      <nav aria-label="On this page" className="mt-4">
        <ul className="flex flex-wrap gap-x-6 text-sm">
          {CONTENTS.map(({ href, label }) => (
            <li key={href}>
              <a href={href} className="inline-flex min-h-11 items-center font-medium underline decoration-foreground/30 underline-offset-4 hover:decoration-foreground">
                {label}
              </a>
            </li>
          ))}
        </ul>
      </nav>

      <section id="glance" aria-labelledby="glance-title" className="mt-10 scroll-mt-20">
        <h2 id="glance-title" className={H2}>At a glance</h2>
        <ComparisonTable />
        <ComparisonList />
        <div className="mt-4 grid gap-1 text-sm text-muted-foreground">
          <p id="partial-note">(partial): Partially stated by the vendor; see source.</p>
          <p id="not-stated-note">—: Not stated publicly as of {COMPARE_ACCESSED}.</p>
        </div>
      </section>

      <section id="fit" aria-labelledby="fit-title" className={SECTION}>
        <h2 id="fit-title" className={H2}>Where each fits</h2>
        <div className="mt-6 grid gap-8 lg:grid-cols-3">
          <div className="border-l pl-5">
            <h3 className="font-semibold">Hosted processors fit better</h3>
            <p className="mt-2 text-sm/6 text-pretty text-muted-foreground">Nothing to run; a fee per payment; they onboard you. They operate the payment infrastructure for you; settlement and payout options vary by provider.</p>
          </div>
          <div className="border-l pl-5">
            <h3 className="font-semibold">BTCPay Server fits better</h3>
            <p className="mt-2 text-sm/6 text-pretty text-muted-foreground">Self-hosted and free; built for Bitcoin. Bitcoin on-chain and Lightning, with community altcoins via plugins.</p>
          </div>
          <div className="border-l pl-5">
            <h3 className="font-semibold">Phala Pay fits</h3>
            <p className="mt-2 text-sm/6 text-pretty text-muted-foreground">Self-hosted and free; built for ERC-20 tokens on Ethereum and Base, with verifiable attestation. You run your own Phala Cloud instance, RPC providers, and backups. There is no fiat settlement or dashboard; you own compliance.</p>
            <a href={LINKS.deploy} className="mt-2 inline-flex min-h-11 items-center text-sm font-medium underline underline-offset-4">Start a testnet instance</a>
          </div>
        </div>
      </section>

      <section id="sources" aria-labelledby="sources-title" className={SECTION}>
        <h2 id="sources-title" className={H2}>Sources</h2>
        <p className="mt-2 text-sm text-muted-foreground">All accessed {COMPARE_ACCESSED}.</p>
        <ol className="mt-6 list-decimal gap-12 pl-6 text-sm/6 marker:text-muted-foreground md:columns-2">
          {sources.map(({ url, title, host, archived }, index) => (
            <li key={url} id={`source-${index + 1}`} className="mb-3 scroll-mt-20 break-inside-avoid pl-1">
              <a href={url} className="underline decoration-foreground/30 underline-offset-4 hover:decoration-foreground">{title}</a>
              <span className="text-muted-foreground"> · {host}{archived && " · read via archive snapshot"}</span>
            </li>
          ))}
        </ol>
      </section>
    </main>
  );
}
