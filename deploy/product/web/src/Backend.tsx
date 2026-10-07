import { ExternalLink, Eye, ReceiptText, Undo2, Webhook } from "lucide-react";
import type { ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardHeader, CardTitle } from "@/components/ui/card";
import { CodeBlock } from "@/components/ui/code-block";
import { Hash } from "@/components/ui/hash";
import { StatusBadge } from "@/components/ui/status-badge";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import type { Account, DepositAddressResponse, Network, PaymentRow, Selection, Timeline, Trust } from "./api.js";
import { DataItem, DataList, Empty, EmptyState, ExplorerLink, LINK, LearnMore, Subsection, TABLE, TOUCH, statusTone, useMediaQuery, useShowAll } from "./common.js";
import { Refunds } from "./Refunds.js";
import { assetOf, networkOf } from "./chains.js";
import { day, dollars, price, signedDollars, statusLabel, time, tokenName, tokens } from "./format.js";
import { Sweeps } from "./Sweeps.js";
import { QueryState, type QueryView } from "./queryView.js";
import { EventStream, EventsLog, LedgerPanel, Requests } from "./Timeline.js";

// The shown payment's row (`aria-current`): an indicator on its left edge.
const SELECTED_ROW = "aria-[current=true]:bg-muted/60 aria-[current=true]:shadow-[inset_2px_0_0_var(--foreground)]";

/**
 * What the merchant's backend sees while its customer pays: the payment's live steps, then its
 * payments, refunds, sweeps, API requests, and the service's attestation. A card beside the
 * customer's view, in the page's flow at its natural height.
 */
export function Backend({
  account: accountView,
  selected,
  timeline: timelineView,
  trust: trustView,
  address: addressView,
  networks: networksView,
  onSelect,
}: {
  account: QueryView<Account>;
  selected: Selection | null;
  timeline: QueryView<Timeline>;
  trust: QueryView<Trust>;
  address: QueryView<DepositAddressResponse>;
  networks: QueryView<Network[]>;
  onSelect: (selection: Selection) => void;
}) {
  const account = accountView.data ?? null;
  const timeline = timelineView.data ?? null;
  const networks = networksView.data;
  const live = timelineView.error === null && (timeline?.steps.some((step) => step.state === "current") ?? selected !== null);
  const order = timeline?.quote?.metadata["order_id"];
  const deposit = timeline?.deposit ?? null;
  const status = selected === null ? "Idle" : timelineView.error !== null ? "Unavailable" : live ? "Live" : "Done";
  return (
    <Card role="complementary" aria-label="Your backend">
      <CardHeader className="items-center gap-y-1 py-3 sm:py-3">
        <div className="flex min-w-0 items-baseline gap-x-3">
          <CardTitle>Your backend</CardTitle>
          <p className="text-sm text-muted-foreground">
            <span
              className={cn(
                "mr-1.5 inline-block size-1.5 rounded-full align-middle",
                status === "Live" ? "bg-success" : status === "Unavailable" ? "bg-destructive" : "bg-muted-foreground",
              )}
              aria-hidden="true"
            />
            <span className="text-foreground" data-testid="stream-status">
              {status}
            </span>
            {selected === null ? "" : ` · ${selected.kind === "quote" ? "quote" : "deposit"}`}
          </p>
        </div>
        <dl className="flex flex-wrap items-center gap-x-4 gap-y-1 text-sm">
          {account !== null && selected === null && <MetaItem label="Workspace" value={account.account_id} testId="meta-workspace" />}
          {selected !== null && order !== undefined && <MetaItem label="Order" value={order} testId="meta-order" />}
          {selected !== null && (
            <MetaItem label={selected.kind === "quote" ? "Quote" : "Deposit"} value={selected.id} testId="meta-selected" />
          )}
        </dl>
      </CardHeader>
      {/* The stepper, one row high; opened steps add their details under it. */}
      <div className="@container px-4 py-2 sm:px-6" aria-live="off">
        <EventStream timeline={timelineView} loading={selected?.id ?? null} />
        {timelineView.data !== undefined && <QueryState view={timelineView} />}
      </div>
      <Tabs defaultValue="credits" className="flex-1 gap-0 border-t">
        <TabsList
          variant="line"
          aria-label="Backend"
          // Five short names that fit a phone's width; should they not, the row wraps.
          className="h-auto w-full flex-wrap justify-start gap-x-2 px-2 sm:px-4"
        >
          <Tab value="credits" count={account?.payments.length}>
            Credits
          </Tab>
          <Tab value="refunds" count={timeline?.refunds.length}>
            Refunds
          </Tab>
          <Tab value="sweeps">Sweeps</Tab>
          <Tab value="api" count={timeline === null ? undefined : timeline.api.length + timeline.events.length}>
            API
          </Tab>
          <Tab value="trust">Trust</Tab>
        </TabsList>
        <TabsContent value="credits" className="flex flex-col gap-3 px-4 py-4 sm:px-6">
          {addressView.data === undefined && <QueryState view={addressView} />}
          {accountView.error === null && (
            <CreditsTab account={account} selected={selected} address={addressView} networks={networks} onSelect={onSelect} />
          )}
          <QueryState view={accountView} />
        </TabsContent>
        <TabsContent value="refunds" className="flex flex-col px-4 py-4 sm:px-6">
          {timeline === null || deposit === null || account === null ? (
            <EmptyState icon={Undo2} title="No deposit to refund">
              A credited payment can be refunded from the treasury once final.
            </EmptyState>
          ) : (
            <div className="flex flex-col gap-6">
              {timeline.ledger !== null && <LedgerPanel ledger={timeline.ledger} />}
              <Refunds timeline={timeline} deposit={deposit} />
            </div>
          )}
        </TabsContent>
        <TabsContent value="sweeps" className="flex flex-col px-4 py-4 sm:px-6">
          <Sweeps ready={account !== null} />
        </TabsContent>
        <TabsContent value="api" className="flex flex-col px-4 py-4 sm:px-6">
          {timeline === null ? (
            <EmptyState icon={Webhook} title="No requests yet">
              The signed webhooks and the API calls of a payment show here.
            </EmptyState>
          ) : (
            <div className="flex flex-col gap-6">
              <EventsLog events={timeline.events} />
              <Requests exchanges={timeline.api} title="API requests" id="api-title" />
            </div>
          )}
        </TabsContent>
        <TabsContent value="trust" className="px-4 py-4 sm:px-6">
          <TrustDetails trust={trustView} networks={networksView} />
        </TabsContent>
      </Tabs>
    </Card>
  );
}

/** A key and its value in the card's header, with a copy button. */
function MetaItem({ label, value, testId }: { label: string; value: string; testId: string }) {
  return (
    <div className="flex items-center gap-2">
      <dt className="text-muted-foreground">{label}</dt>
      <dd data-testid={testId}>
        <Hash value={value} copyLabel={`Copy ${label.toLowerCase()} id`} />
      </dd>
    </div>
  );
}

/**
 * A tab, with its count as plain text once there is one, from sm: on a phone the five tabs fit the
 * row without them.
 */
function Tab({ value, count, children }: { value: string; count?: number | undefined; children: string }) {
  return (
    <TabsTrigger value={value} className="h-11 min-w-11 flex-none px-2 tabular-nums">
      {children}
      {count !== undefined && count > 0 && <span className="max-sm:hidden">({count})</span>}
    </TabsTrigger>
  );
}

/**
 * Shows a payment in the timeline above; the shown one's row (marked `aria-current`) says
 * "Viewing" instead, in the button's place and at its size, as the button's selected state.
 */
function ViewButton({ selected, id, onClick }: { selected: boolean; id: string; onClick: () => void }) {
  if (selected) {
    return (
      <span className={cn("inline-flex h-8 items-center gap-1.5 rounded-md bg-muted px-3 text-sm font-medium", TOUCH)}>
        <Eye className="size-4 text-muted-foreground" aria-hidden="true" />
        Viewing
      </span>
    );
  }
  return (
    <Button type="button" variant="secondary" size="sm" className={TOUCH} onClick={onClick} aria-label={`View ${id}`}>
      View
    </Button>
  );
}

function CreditsTab({
  account,
  selected,
  address: addressView,
  networks,
  onSelect,
}: {
  account: Account | null;
  selected: Selection | null;
  address: QueryView<DepositAddressResponse>;
  networks: Network[] | undefined;
  onSelect: (selection: Selection) => void;
}) {
  const address = addressView.data ?? null;
  // A table where there is room for its columns; on a phone each payment as a list item.
  const wide = useMediaQuery("(min-width: 48rem)");
  // The most recent payments and balance lines (both newest first); the rest on request.
  const recentPayments = useShowAll(account?.payments ?? [], 3);
  const recentLines = useShowAll(account?.ledger ?? [], 2);
  if (account === null) {
    return <Empty>Loading…</Empty>;
  }
  if (account.payments.length === 0 && account.ledger.length === 0 && address === null) {
    return (
      <EmptyState icon={ReceiptText} title="No payments yet">
        Pay in the customer view; each payment shows here with its status and credit.
      </EmptyState>
    );
  }
  const payments = recentPayments.shown.map((row) => {
    const network = networkOf(networks, row.chain_id);
    const selection: Selection = row.id.startsWith("dep_") ? { kind: "deposit", id: row.id } : { kind: "quote", id: row.id };
    return {
      row,
      symbol: (row.asset ?? "token").toUpperCase(),
      decimals: assetOf(network, row.asset)?.decimals,
      selection,
      isSelected: selected?.id === row.id || selected?.id === row.quote,
    };
  });
  return (
    <div className="flex flex-col gap-8">
      {payments.length > 0 && (
        <section aria-label="Credits" className="flex flex-col gap-2">
          {wide ? (
            // The first column clears the shown row's indicator.
            <Table className={cn(TABLE, "[&_tr>*:first-child]:pl-3")}>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  {/* The payment takes the row's free width; the other columns, and the action at
                      the end, keep to their content beside it. */}
                  <TableHead scope="col" className="w-full">Payment</TableHead>
                  <TableHead scope="col">Status</TableHead>
                  <TableHead scope="col" className="text-right">
                    Credited
                  </TableHead>
                  <TableHead scope="col" className="text-right">
                    Net
                  </TableHead>
                  <TableHead scope="col">
                    <span className="sr-only">Timeline</span>
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody className="tabular-nums">
                {payments.map(({ row, symbol, decimals, selection, isSelected }) => (
                  <TableRow
                    key={row.id}
                    data-testid="payment"
                    data-kind={row.kind}
                    aria-current={isSelected ? "true" : undefined}
                    className={SELECTED_ROW}
                  >
                    <TableCell className="whitespace-normal">
                      <PaymentAmount row={row} symbol={symbol} decimals={decimals} />
                    </TableCell>
                    <TableCell>
                      <PaymentStatus row={row} />
                    </TableCell>
                    <TableCell className="text-right">
                      <Credit row={row} />
                    </TableCell>
                    <TableCell className="text-right">
                      <Net row={row} symbol={symbol} decimals={decimals} />
                    </TableCell>
                    <TableCell className="text-right">
                      <ViewButton selected={isSelected} id={row.id} onClick={() => onSelect(selection)} />
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          ) : (
            <ul className="flex flex-col divide-y border-y text-sm tabular-nums">
              {payments.map(({ row, symbol, decimals, selection, isSelected }) => (
                <li
                  key={row.id}
                  data-testid="payment"
                  data-kind={row.kind}
                  aria-current={isSelected ? "true" : undefined}
                  className={cn("flex flex-col gap-3 px-3 py-3", SELECTED_ROW)}
                >
                  <div className="flex items-start justify-between gap-3">
                    <div className="min-w-0">
                      <PaymentAmount row={row} symbol={symbol} decimals={decimals} />
                    </div>
                    <ViewButton selected={isSelected} id={row.id} onClick={() => onSelect(selection)} />
                  </div>
                  <dl className="grid grid-cols-[auto_minmax(0,1fr)_minmax(0,1fr)] gap-x-4 gap-y-3">
                    <div>
                      <dt className="text-muted-foreground">Status</dt>
                      <dd className="mt-1">
                        <PaymentStatus row={row} />
                      </dd>
                    </div>
                    <div>
                      <dt className="text-muted-foreground">Credited</dt>
                      <dd className="mt-1">
                        <Credit row={row} />
                      </dd>
                    </div>
                    <div>
                      <dt className="text-muted-foreground">Net</dt>
                      <dd className="mt-1">
                        <Net row={row} symbol={symbol} decimals={decimals} />
                      </dd>
                    </div>
                  </dl>
                </li>
              ))}
            </ul>
          )}
          {recentPayments.toggle}
        </section>
      )}
      {account.ledger.length > 0 && (
        <Subsection title="How this balance adds up" id="balance-lines-title">
          <p className="text-sm text-pretty text-muted-foreground">
            A bonus is this demo merchant's own promotion, not Phala Pay's.{" "}
            <LearnMore anchor="promotions-are-your-own-logic" topic="promotions in your ledger" />
          </p>
          {/* Each line: what moved the balance and by how much; under it, when and for which deposit. */}
          <ul className="flex flex-col divide-y border-y text-sm" aria-label="Balance lines">
            {recentLines.shown.map((line) => (
              <li key={`${line.deposit}-${line.kind}-${line.reason}-${line.at}`} data-testid="ledger-line" data-kind={line.kind}
                className="flex flex-col gap-1 py-3">
                <p className="flex items-baseline justify-between gap-4">
                  <span>
                    <span className="font-medium">{line.kind === "bonus" ? "Bonus" : "Credit"}</span>
                    <span className="text-muted-foreground"> · </span>
                    {/* Event names in mono; a bonus grant's label is prose. */}
                    <span className={cn(line.reason.startsWith("deposit.") && "font-mono text-mono")}>{line.reason}</span>
                  </span>
                  <span className="font-medium tabular-nums">{signedDollars(line.amount)}</span>
                </p>
                <p className="text-muted-foreground">
                  <span className="tabular-nums" title={time(line.at)}>{day(line.at)}</span> · <Hash value={line.deposit} />
                </p>
              </li>
            ))}
          </ul>
          {recentLines.toggle}
        </Subsection>
      )}
      {address !== null && (
        <div className="flex flex-col gap-2">
          <AddressView account={account} address={address} networks={networks} selected={selected} onSelect={onSelect} />
          <QueryState view={addressView} />
        </div>
      )}
    </div>
  );
}

/** A payment's amount and the rate it was valued at, then its kind and when it was created. */
function PaymentAmount({ row, symbol, decimals }: { row: PaymentRow; symbol: string; decimals: number | undefined }) {
  return (
    <>
      <div>
        <span className="font-medium">{tokens(row.amount_atomic, symbol, decimals)}</span>
        {row.exchange_rate !== null && (
          <span className="text-muted-foreground"> at {price(row.exchange_rate)} / {symbol}</span>
        )}
      </div>
      <div className="text-muted-foreground">
        {row.kind === "quote" ? "Quote" : "Deposit address"} · <span title={time(row.created)}>{day(row.created)}</span>
      </div>
    </>
  );
}

/** A payment's status, then whether it is final and swept, as text. */
function PaymentStatus({ row }: { row: PaymentRow }) {
  const meta = [row.final ? "final" : null, row.swept ? "swept" : null].filter((each) => each !== null);
  return (
    <div className="flex flex-col items-start gap-1">
      <StatusBadge tone={statusTone(row.status)}>{statusLabel(row.status)}</StatusBadge>
      {meta.length > 0 && <span className="text-muted-foreground">{meta.join(" · ")}</span>}
    </div>
  );
}

function Credit({ row }: { row: PaymentRow }) {
  return (
    <>
      <div>{row.amount === null || row.tx_hash === null ? "—" : dollars(row.amount)}</div>
      {row.bonus !== null && row.bonus !== 0 && <div className="text-muted-foreground">{signedDollars(row.bonus)} bonus</div>}
    </>
  );
}

function Net({ row, symbol, decimals }: { row: PaymentRow; symbol: string; decimals: number | undefined }) {
  return (
    <>
      <div className="font-medium">{row.net === null ? "—" : dollars(row.net)}</div>
      {row.amount_refunded_atomic !== "0" && (
        <div className="text-muted-foreground">−{tokens(row.amount_refunded_atomic, symbol, decimals)} refunded</div>
      )}
    </>
  );
}

/** The deposit address as the product sees it: checked against its pins, and its payments. */
function AddressView({
  account,
  address,
  networks,
  selected,
  onSelect,
}: {
  account: Account;
  address: DepositAddressResponse;
  networks: Network[] | undefined;
  selected: Selection | null;
  onSelect: (selection: Selection) => void;
}) {
  const view = address.deposit_address;
  // Each of the address's networks as the product's selectors name it and its tokens.
  const named = (chainId: number) => networks?.find((each) => each.chain_id === chainId);
  const recent = useShowAll(view.payments, 3);
  return (
    <Subsection
      title="Deposit address"
      id="address-title"
      aside={
        address.verified ? (
          <StatusBadge tone="success" data-testid="deposit-address-verified">
            Verified
          </StatusBadge>
        ) : (
          <StatusBadge tone="danger">Not verified</StatusBadge>
        )
      }
    >
      <p className="text-sm text-pretty text-muted-foreground">
        {address.verified
          ? "The product's SDK recomputed it from its pins before showing it."
          : "The address does not match what the product's pins derive; it is not shown to the customer."}
      </p>
      <DataList className="divide-y border-y">
        {view.address !== null ? (
          <DataItem label="Address (every network)" data-testid="deposit-address">
            <ExplorerLink chainId={view.networks[0]?.chain_id} kind="address" value={view.address} copy />
          </DataItem>
        ) : (
          view.networks.map((network) => (
            <DataItem key={network.chain_id} label={`Address on ${named(network.chain_id)?.name ?? `chain ${network.chain_id}`}`}>
              <ExplorerLink chainId={network.chain_id} kind="address" value={network.address} copy />
            </DataItem>
          ))
        )}
        {view.networks.map((network) => {
          const known = named(network.chain_id);
          return (
            <DataItem key={network.chain_id} label={known?.name ?? `Chain ${network.chain_id}`} data-testid="deposit-address-network">
              {network.assets.map((asset) => tokenName(asset.asset.toUpperCase(), known?.testnet ?? true)).join(", ")}
            </DataItem>
          );
        })}
        <DataItem label="Metadata">
          <CodeBlock value={JSON.stringify(view.metadata, null, 2)} label="metadata" />
        </DataItem>
      </DataList>
      {view.payments.length === 0 ? (
        <Empty>No payments yet. Send any amount of a supported token to the address.</Empty>
      ) : (
        <ul className="flex flex-col divide-y border-y text-sm" aria-label="Payments the product sees" aria-live="polite">
          {recent.shown.map((payment) => {
            // The deposit's valuation, once the service recorded it.
            const rate = account.payments.find((row) => row.id === payment.deposit)?.exchange_rate ?? null;
            const token = assetOf(named(payment.chain_id), payment.asset);
            const symbol = (payment.asset ?? "token").toUpperCase();
            const isSelected = selected?.id === payment.deposit;
            return (
              <li
                key={payment.deposit}
                data-testid="address-payment"
                aria-current={isSelected ? "true" : undefined}
                className={cn("flex items-center gap-3 px-3 py-2", SELECTED_ROW)}
              >
                <span className="flex min-w-0 flex-1 flex-col items-start">
                  <span>
                    <span className="font-medium tabular-nums">{tokens(payment.amount_atomic, symbol, token?.decimals)}</span>{" "}
                    <span className="text-muted-foreground">
                      {payment.status === "seen"
                        ? `received, ${payment.confirmations ?? 0} confirmation${payment.confirmations === 1 ? "" : "s"}`
                        : rate === null
                          ? "recorded as a deposit"
                          : `credited at ${price(rate)} / ${symbol}`}
                    </span>
                  </span>
                  <ExplorerLink chainId={payment.chain_id} kind="tx" value={payment.tx_hash} />
                </span>
                <ViewButton
                  selected={isSelected}
                  id={payment.deposit}
                  onClick={() => onSelect({ kind: "deposit", id: payment.deposit })}
                />
              </li>
            );
          })}
        </ul>
      )}
      {recent.toggle}
    </Subsection>
  );
}

/**
 * Why the merchant can trust the service, a row each: its attestation, the application's identity
 * (from the TLS certificate's evidence quote), and its custody; then where to verify it.
 */
function TrustDetails({ trust: trustView, networks: networksView }: {
  trust: QueryView<Trust>;
  networks: QueryView<Network[]>;
}) {
  const trust = trustView.data;
  const networks = networksView.data;
  const attestation = trust?.attestation;
  const evidence = trust?.tls_evidence;
  return (
    <div className="flex flex-col gap-3">
      <DataList className="divide-y border-y">
        <TrustItem title="Attestation">
          {attestation === undefined ? (
            trustView.error !== null ? (
              <QueryState view={trustView} />
            ) : (
              <p className="text-muted-foreground" role="status">
                Loading…
              </p>
            )
          ) : attestation.binding_verified ? (
            <div className="flex flex-col gap-1 text-pretty text-muted-foreground">
              <p>
                <StatusBadge tone="success" className="mr-2 align-middle">Attestation verified</StatusBadge>
                Verified for a fresh nonce.{" "}
                <LearnMore anchor="53-pin-your-accounts-webhook-keys" topic="pinning the webhook key" />
              </p>
              <p>
                It binds the key that signs every webhook: <Hash value={attestation.webhook_public_key ?? ""} />
              </p>
            </div>
          ) : (
            <p className="text-muted-foreground">
              <StatusBadge tone="danger" className="mr-2 align-middle">Not verified</StatusBadge>
              The attestation did not bind its keys.
            </p>
          )}
        </TrustItem>
        {evidence == null ? (
          <TrustItem title="Application">
            <p className="text-muted-foreground">TLS evidence unavailable.</p>
          </TrustItem>
        ) : (
          <>
            <TrustItem title="App id">
              <Hash value={evidence.app_id} copyLabel="Copy app id" />
            </TrustItem>
            {evidence.compose_hash !== undefined && (
              <TrustItem title="Compose hash">
                <Hash value={evidence.compose_hash} copyLabel="Copy compose hash" />
              </TrustItem>
            )}
          </>
        )}
        <TrustItem title="Custody">
          <p className="text-pretty">
            Every address pays only the merchant's treasury; Phala Pay holds no funds.
            {networksView.error === null && networks !== undefined && (
              <span className="text-muted-foreground"> On {networks.map((each) => each.name).join(" and ")}.</span>
            )}
          </p>
          <QueryState view={networksView} />
        </TrustItem>
      </DataList>
      {trustView.data !== undefined && <QueryState view={trustView} />}
      {trust !== undefined && (
        <div className="flex flex-wrap gap-x-6">
          <TrustLink href={trust.verify_docs}>Attestation guide</TrustLink>
          <TrustLink href={trust.dstack_verifier}>dstack verifier</TrustLink>
        </div>
      )}
    </div>
  );
}

/** A row: its name beside its value (above it on a phone). */
function TrustItem({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="grid gap-x-4 gap-y-1 py-3 sm:grid-cols-[7rem_minmax(0,1fr)]">
      <dt className="font-medium">{title}</dt>
      <dd className="flex min-w-0 flex-col items-start gap-1">{children}</dd>
    </div>
  );
}

/** A documentation link, off the page, as tall as a touch target. */
function TrustLink({ href, children }: { href: string; children: ReactNode }) {
  return (
    <a className={cn(LINK, "inline-flex min-h-11 items-center gap-1.5 text-sm")} href={href} target="_blank" rel="noreferrer">
      {children}
      <ExternalLink className="size-3.5 text-muted-foreground" aria-hidden="true" />
    </a>
  );
}
