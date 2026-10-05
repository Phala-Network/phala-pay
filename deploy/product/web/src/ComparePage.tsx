import { Table, TableBody, TableCaption, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { COMPARE_ACCESSED, competitors, dimensions, phalaPay, sources, type Cell } from "./content/compare.js";
import { CONTAINER, LINKS } from "./Site.js";

export function ComparisonCell({ cell }: { cell: Cell }) {
  if (cell.status === "not-stated") return <span aria-describedby="not-stated-note">—</span>;
  const sourceNumber = sources.findIndex(({ url }) => url === cell.source) + 1;
  return (
    <>
      {cell.text}
      {cell.status === "partially" && <sup className="ml-1"><a href="#partial-note" aria-label="Partially stated by the vendor">†</a></sup>}
      {sourceNumber > 0 && <a href={`#source-${sourceNumber}`} className="ml-1 text-xs underline underline-offset-4" aria-label={`Source ${sourceNumber}`}>[{sourceNumber}]</a>}
    </>
  );
}

export function ComparePage() {
  const vendors = [phalaPay, ...competitors];
  return (
    <main id="top" className={`${CONTAINER} flex-1 py-16`}>
      <h1 className="text-3xl font-semibold tracking-tight sm:text-4xl">How Phala Pay compares</h1>
      <p className="mt-4 text-sm leading-6 text-muted-foreground">Last checked {COMPARE_ACCESSED}. Competitor terms change; check their sites before deciding.</p>
      <section aria-labelledby="glance-title" className="mt-12">
        <h2 id="glance-title" className="text-2xl font-semibold tracking-tight">At a glance</h2>
        <div className="mt-6 rounded-xl border bg-card">
          <Table className="min-w-[90rem] table-fixed">
            <TableCaption className="sr-only">Phala Pay and five crypto payment services, compared across ten dimensions.</TableCaption>
            <TableHeader>
              <TableRow>
                <TableHead scope="col" className="w-44 whitespace-normal p-4">Dimension</TableHead>
                {vendors.map(({ id, name }) => <TableHead scope="col" key={id} className={`whitespace-normal p-4 ${id === phalaPay.id ? "bg-muted/50" : ""}`}>{name}</TableHead>)}
              </TableRow>
            </TableHeader>
            <TableBody>
              {dimensions.map(({ key, label }) => (
                <TableRow key={key}>
                  <TableHead scope="row" className="whitespace-normal p-4 align-top">{label}</TableHead>
                  {vendors.map((vendor) => <TableCell key={vendor.id} className={`whitespace-normal p-4 align-top leading-6 ${vendor.id === phalaPay.id ? "bg-muted/50" : ""}`}><ComparisonCell cell={vendor[key]} /></TableCell>)}
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
        <p id="partial-note" className="mt-4 text-sm text-muted-foreground">† Partially stated by the vendor; see source.</p>
        <p id="not-stated-note" className="mt-2 text-sm text-muted-foreground">— Not stated publicly as of {COMPARE_ACCESSED}.</p>
      </section>
      <div className="mt-12 grid gap-8 lg:grid-cols-3">
        <section aria-labelledby="hosted-title">
          <h2 id="hosted-title" className="text-xl font-semibold tracking-tight">Where hosted processors fit better</h2>
          <p className="mt-3 text-sm leading-6 text-muted-foreground">Nothing to run; a fee per payment; they onboard you.</p>
        </section>
        <section aria-labelledby="btcpay-title">
          <h2 id="btcpay-title" className="text-xl font-semibold tracking-tight">Where BTCPay Server fits better</h2>
          <p className="mt-3 text-sm leading-6 text-muted-foreground">Self-hosted and free; built for Bitcoin. Bitcoin on-chain and Lightning, with community altcoins via plugins.</p>
        </section>
        <section aria-labelledby="phala-title">
          <h2 id="phala-title" className="text-xl font-semibold tracking-tight">Where Phala Pay fits</h2>
          <p className="mt-3 text-sm leading-6 text-muted-foreground">Self-hosted and free; built for ERC-20 tokens on Ethereum and Base, with verifiable attestation. You run your own Phala Cloud instance, RPC providers, and backups. There is no fiat settlement or dashboard; you own compliance.</p>
          <a href={LINKS.deploy} className="mt-4 inline-block text-sm font-medium underline underline-offset-4">Start a testnet instance</a>
        </section>
      </div>
      <section aria-labelledby="sources-title" className="mt-12 border-t pt-12">
        <h2 id="sources-title" className="text-2xl font-semibold tracking-tight">Sources and last checked date</h2>
        <ol className="mt-6 list-decimal space-y-3 pl-6 text-sm leading-6">
          {sources.map(({ url, archived }, index) => (
            <li key={url} id={`source-${index + 1}`} className="scroll-mt-24">
              <a href={url} className="break-all underline underline-offset-4">{url}</a>
              {archived && " (read via archive snapshot)"}
              <span className="text-muted-foreground"> · Accessed {COMPARE_ACCESSED}</span>
            </li>
          ))}
        </ol>
      </section>
    </main>
  );
}
