import { Check, ChevronDown, ChevronRight, X } from "lucide-react";
import { CodeBlock } from "@/components/ui/code-block";
import { Hash } from "@/components/ui/hash";
import { StatusBadge } from "@/components/ui/status-badge";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { cn } from "@/lib/utils";
import type {
  ApiExchange,
  Detail as StepDetail,
  LedgerView,
  Step,
  StepKey,
  Timeline,
  WebhookEvent,
} from "./api.js";
import { DataItem, DataList, Empty, ExplorerLink, Subsection, TABLE } from "./common.js";
import { assetOf, networkOf } from "./chains.js";
import { approx, clock, dollars, duration, minusDollars, rate, short, signedDollars, time, tokens } from "./format.js";
import { QueryState, type QueryView } from "./queryView.js";
import { useNetworks } from "./queries.js";

// Each step's title, what it waits for, and the time it usually takes: the hints are expectations,
// every time shown next to them is real (the chain's block time, the service's timestamps, or when
// your server received a webhook).
// A step that usually takes a known time says so on its line until it happens (`usually`); the
// credit's comes from the chain's `typical_credit_seconds` (`GET /v1/config`).
const STEP_COPY: Record<StepKey, { title: string; hint: string; failed?: string; usually?: string }> = {
  quote_created: {
    title: "Quote created",
    hint: "A locked price for an exact amount, for 15 minutes.",
  },
  sent: {
    title: "Sent on chain",
    hint: "Waiting for a transfer to the address.",
    failed: "The quote expired without a payment.",
  },
  received: {
    title: "Received by Phala Pay",
    hint:
      "Usually within a block of sending, about 12 s on Sepolia and 2 s on Base Sepolia: the " +
      "service scans every new block for its addresses.",
  },
  credited: {
    title: "Credited",
    hint:
      "At the chain's confirmation: 2 blocks on Sepolia, about 30 s after sending, and 3 blocks on " +
      "Base Sepolia, about 7 s. Once both RPC providers report the same block, the deposit is " +
      "valued and screened.",
    failed: "The deposit was rejected and will not be credited.",
  },
  webhook_received: {
    title: "Webhook handled by your server",
    hint: "The signed deposit.credited moves the balance; its metadata arrives with it.",
  },
  final: {
    usually: "~15 min",
    title: "Final",
    hint:
      "About 15 minutes on Ethereum mainnet; this demo runs on testnets. Until then a reorg that replaces the transaction reverses the " +
      "credit; only a final deposit can be refunded. The time shown is the deposit's final_at.",
  },
  reversed: {
    title: "Reversed",
    hint: "The transaction left the chain before finality; deposit.reversed took the credit back.",
    failed: "The transaction left the chain before finality; deposit.reversed took the credit back.",
  },
  swept: {
    title: "Swept to the treasury",
    hint:
      "Phala Pay never sweeps: the merchant signs factory.flush from its own wallet or Safe (see " +
      "Sweeps below). The deposit is marked swept once that flush is finalized.",
  },
};

// A step's line in columns: its dot, its title with the opener, and its time on the right: the
// time since sending for the steps after it, else the time it happened; for a step yet to happen,
// how long it usually takes. Each line is a 44px target.
const STEP_GRID = "grid grid-cols-[1rem_minmax(0,1fr)_auto] items-center gap-x-3";

// A quote's steps, shown before there is a payment to follow.
const PREVIEW: StepKey[] = ["quote_created", "sent", "received", "credited", "webhook_received", "final", "swept"];

/**
 * The payment's steps as a compact live log: one line per step, with its status and its real
 * time; each line opens to what it waits for and its data.
 */
export function EventStream({ timeline: timelineView, loading }: { timeline: QueryView<Timeline>; loading: string | null }) {
  const timeline = timelineView.data ?? null;
  const networks = useNetworks().data;
  // The payment's own chain and token, for its amounts and links.
  const chainId = timeline?.deposit?.chain_id ?? timeline?.quote?.chain_id;
  const asset = timeline?.deposit?.asset ?? timeline?.quote?.asset ?? null;
  const network = networkOf(networks, chainId);
  const token: StepToken = {
    chainId,
    symbol: (asset ?? "").toUpperCase(),
    decimals: assetOf(network, asset)?.decimals ?? 18,
    // The confirmation is per chain, so before there is a token any of the chain's assets has it.
    creditSeconds: (assetOf(network, asset) ?? network?.assets[0])?.typical_credit_seconds ?? null,
  };
  if (loading === null) {
    return (
      <div className="flex flex-col gap-3">
        <p className="text-sm text-pretty text-muted-foreground">
          Each step of a payment, as your backend receives it, with real times from the chain and the service.
        </p>
        <ol className="flex flex-col" aria-label="The steps of a payment">
          {PREVIEW.map((key) => (
            <StreamStep key={key} step={{ key, state: "upcoming", at: null, details: [] }} sent={null} token={token} />
          ))}
        </ol>
      </div>
    );
  }
  if (timeline === null) {
    if (timelineView.error !== null) {
      return <QueryState view={timelineView} />;
    }
    return (
      <ol className="flex flex-col" aria-label={`Loading ${short(loading)}`} aria-busy="true">
        {PREVIEW.map((key) => (
          <li key={key} className={cn(STEP_GRID, "h-11 px-2")}>
            <span className="size-2.5 justify-self-center rounded-full bg-muted motion-safe:animate-pulse" />
            <span className="h-2.5 w-40 rounded-full bg-muted motion-safe:animate-pulse" />
          </li>
        ))}
      </ol>
    );
  }
  return (
    <ol className="flex flex-col" aria-label="Payment timeline">
      {timeline.steps.map((step, index) => (
        <StreamStep
          key={step.key}
          step={step}
          sent={timeline.sent?.at ?? null}
          // The current step shows when it started: when the step before it completed.
          since={
            step.state === "current" && step.at === null
              ? Math.max(0, ...timeline.steps.slice(0, index).map((each) => each.at ?? 0)) || null
              : null
          }
          token={token}
        />
      ))}
    </ol>
  );
}

function StepDot({ state }: { state: Step["state"] }) {
  if (state === "current") {
    return (
      <span className="flex size-4 items-center justify-center" aria-hidden="true">
        <span className="size-2.5 rounded-full bg-brand ring-2 ring-foreground/20" />
      </span>
    );
  }
  return (
    <span
      className={cn(
        "flex size-4 items-center justify-center rounded-full bg-card transition-colors motion-reduce:transition-none",
        state === "complete" && "bg-foreground text-background",
        state === "failed" && "bg-destructive text-background",
        state === "upcoming" && "border border-dashed border-muted-foreground",
      )}
      aria-hidden="true"
    >
      {state === "complete" && <Check className="size-2.5" strokeWidth={3.5} />}
      {state === "failed" && <X className="size-2.5" strokeWidth={3.5} />}
    </span>
  );
}

/** The followed payment's chain and token. */
interface StepToken {
  chainId: number | undefined;
  symbol: string;
  decimals: number;
  /** The chain's typical credit time, for the credit step's `usually`; null until known. */
  creditSeconds: number | null;
}

function StreamStep({
  step,
  sent,
  since = null,
  token,
}: {
  step: Step;
  sent: number | null;
  since?: number | null;
  token: StepToken;
}) {
  const copy = STEP_COPY[step.key];
  const usually =
    step.key === "credited" ? (token.creditSeconds === null ? undefined : approx(token.creditSeconds)) : copy.usually;
  // Seconds since the payment was sent, for the steps after it.
  const elapsed =
    step.at !== null && sent !== null && step.key !== "sent" && step.key !== "quote_created" ? step.at - sent : null;
  const failed = step.state === "failed" && copy.failed !== undefined;
  return (
    // The rail from this step's dot to the next one's, through the dots' centres (8px of padding
    // and half a dot in), past this step's opened details.
    <li
      className="relative before:absolute before:top-7.5 before:-bottom-3.5 before:left-4 before:w-px before:-translate-x-1/2 before:bg-border last:before:hidden"
      data-step={step.key}
      data-state={step.state}
      aria-current={step.state === "current" ? "step" : undefined}
    >
      <Collapsible>
        <CollapsibleTrigger
          className={cn(
            STEP_GRID,
            "group/trigger min-h-11 w-full rounded-md px-2 py-1.5 text-left transition-colors hover:bg-muted/60 motion-reduce:transition-none",
          )}
        >
          <StepDot state={step.state} />
          <span className="flex min-w-0 items-center gap-1.5">
            <span
              className={cn(
                "min-w-0 text-sm text-pretty",
                step.state === "upcoming" && "text-muted-foreground",
                step.state === "current" && "font-medium",
                step.state === "failed" && "text-destructive",
              )}
            >
              {copy.title}
              <span className="sr-only">, {stateLabel(step.state)}</span>
            </span>
            <ChevronDown
              className="size-4 shrink-0 text-muted-foreground transition-transform group-data-[state=open]/trigger:rotate-180 motion-reduce:transition-none"
              aria-hidden="true"
            />
          </span>
          <span className="text-right text-sm whitespace-nowrap text-muted-foreground tabular-nums">
            {elapsed !== null ? (
              `+${duration(elapsed)}`
            ) : step.at !== null ? (
              <time dateTime={new Date(step.at * 1000).toISOString()}>{clock(step.at)}</time>
            ) : (step.state === "upcoming" || step.state === "current") && usually !== undefined ? (
              `usually ${usually}`
            ) : since !== null ? (
              <time dateTime={new Date(since * 1000).toISOString()}>since {clock(since)}</time>
            ) : null}
          </span>
        </CollapsibleTrigger>
        {failed && <p className="pb-1 pl-9 text-sm text-pretty text-destructive">{copy.failed}</p>}
        {/* Under the title: 8px of padding, the dot, and the gap. */}
        <CollapsibleContent className="flex flex-col gap-2 pr-2 pb-3 pl-9 text-sm">
          <p className="text-pretty text-muted-foreground">{copy.hint}</p>
          {step.at !== null && (
            <p className="tabular-nums" data-testid="step-time">
              {time(step.at)}
              {elapsed !== null && <span className="text-muted-foreground"> · {duration(elapsed)} after sending</span>}
            </p>
          )}
          {step.details.length > 0 && (
            <DataList className="border-l pl-3">
              {step.details.map((detail) => (
                <DataItem key={detail.label} label={detail.label}>
                  <DetailValue detail={detail} token={token} />
                </DataItem>
              ))}
            </DataList>
          )}
        </CollapsibleContent>
      </Collapsible>
    </li>
  );
}

function DetailValue({ detail, token }: { detail: StepDetail; token: StepToken }) {
  const { value, kind } = detail;
  if (value === null) {
    return <>—</>;
  }
  if (kind === "json") {
    return <CodeBlock value={JSON.stringify(value, null, 2)} label={detail.label} />;
  }
  if (typeof value === "object") {
    return <>{JSON.stringify(value)}</>;
  }
  if ((kind === "address" || kind === "tx") && typeof value === "string") {
    return <ExplorerLink chainId={token.chainId} kind={kind} value={value} copy />;
  }
  if ((kind === "id" || kind === "hash") && typeof value === "string") {
    return <Hash value={value} copyLabel={`Copy ${detail.label.toLowerCase()}`} />;
  }
  if (kind === "time" && typeof value === "number") {
    return <>{time(value)}</>;
  }
  if (kind === "usd" && typeof value === "number") {
    return <span className="tabular-nums">{dollars(value)}</span>;
  }
  if (kind === "usd_delta" && typeof value === "number") {
    return <span className="text-success tabular-nums">{signedDollars(value)}</span>;
  }
  if (kind === "atomic" && typeof value === "string") {
    return <span className="tabular-nums">{tokens(value, token.symbol, token.decimals)}</span>;
  }
  if (kind === "rate" && typeof value === "string") {
    return <span className="tabular-nums">{rate(detail.unit ?? token.symbol, value)}</span>;
  }
  return <>{String(value)}</>;
}

/** The deposit's ledger: the service's amounts, what they net to, and what the product's server holds. */
export function LedgerPanel({ ledger }: { ledger: LedgerView }) {
  const product = ledger.product;
  return (
    <Subsection title="Ledger" id="ledger-title">
      <p className="text-sm text-pretty text-muted-foreground">
        While <code className="font-mono text-[13px]">credited</code> or{" "}
        <code className="font-mono text-[13px]">reversed</code>, a deposit nets to{" "}
        <code className="font-mono text-[13px]">amount − amount_refunded − amount_reversed</code>; otherwise to 0.
        Every <code className="font-mono text-[13px]">deposit.*</code> event carries these cumulative amounts, so
        the result does not depend on the order events arrive in.
      </p>
      <DataList data-testid="ledger" className="divide-y border-y">
        <DataItem label="Status" className="font-mono text-[13px]">
          {ledger.status}
        </DataItem>
        <DataItem label="Amount" className="font-mono text-[13px] tabular-nums">
          {ledger.amount === null ? "—" : dollars(ledger.amount)}
        </DataItem>
        <DataItem label="Refunded" className="font-mono text-[13px] tabular-nums">
          {minusDollars(ledger.amount_refunded)}
        </DataItem>
        <DataItem label="Reversed" className="font-mono text-[13px] tabular-nums">
          {minusDollars(ledger.amount_reversed)}
        </DataItem>
        <DataItem label="Nets to" className="font-mono text-[13px] font-semibold tabular-nums" data-testid="nets-to">
          {dollars(ledger.nets_to)}
        </DataItem>
        <DataItem label="Your server's ledger" className="tabular-nums" data-testid="console-net">
          {product === null || product.status === null
            ? "no order yet"
            : product.net === null
              ? `${product.status}${product.reason === null ? "" : ` (${product.reason})`}`
              : `${dollars(product.net)} (credit ${dollars(product.credit ?? 0)}${product.adjustments
                  .map((adjustment) => `, ${signedDollars(adjustment.amount)} by ${adjustment.reason}`)
                  .join("")})`}
        </DataItem>
        {product?.bonus != null && product.bonus !== 0 && (
          <DataItem label="Bonus (this demo's)" className="tabular-nums" data-testid="console-bonus">
            {signedDollars(product.bonus)}, its promotion's share of what the credit nets to
          </DataItem>
        )}
      </DataList>
    </Subsection>
  );
}

/** The webhook events the product's server received; their ids where there is room. */
export function EventsLog({ events }: { events: WebhookEvent[] }) {
  return (
    <Subsection title="Webhook events received" id="events-title">
      {events.length === 0 ? (
        <Empty>None yet.</Empty>
      ) : (
        <Table className={TABLE}>
          <TableHeader>
            <TableRow className="hover:bg-transparent">
              <TableHead scope="col">Type</TableHead>
              <TableHead scope="col" className="hidden md:table-cell">
                Event id
              </TableHead>
              <TableHead scope="col">Received</TableHead>
              <TableHead scope="col">Signature</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {events.map((event) => (
              <TableRow key={event.id} data-testid="webhook-event">
                <TableCell className="font-mono text-[13px]">{event.type}</TableCell>
                <TableCell className="hidden md:table-cell">
                  <Hash value={event.id} />
                </TableCell>
                <TableCell className="text-muted-foreground tabular-nums" title={time(event.received_at)}>
                  {clock(event.received_at)}
                </TableCell>
                <TableCell>{event.verified ? "Verified" : "—"}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
    </Subsection>
  );
}

/** The product's API requests: sent from its server with its restricted key, never the browser. */
export function Requests({ exchanges, title, id }: { exchanges: ApiExchange[]; title: string; id: string }) {
  return (
    <Subsection title={`${title} (${exchanges.length})`} id={id}>
      <p className="text-sm text-pretty text-muted-foreground">
        Sent from the product's server with its restricted API key; the browser never holds it.
      </p>
      {exchanges.length === 0 ? (
        <Empty>None yet.</Empty>
      ) : (
        <ul className="flex flex-col divide-y border-y">
          {exchanges.map((exchange, index) => {
            const url = new URL(exchange.url);
            return (
              <li key={`${exchange.method}-${exchange.url}-${index}`}>
                <details className="group/exchange">
                  <summary className="flex min-h-11 cursor-pointer list-none items-center gap-2 px-2 py-2 transition-colors hover:bg-muted/60 [&::-webkit-details-marker]:hidden">
                    <ChevronRight
                      className="size-4 shrink-0 text-muted-foreground transition-transform group-open/exchange:rotate-90 motion-reduce:transition-none"
                      aria-hidden="true"
                    />
                    <code className="min-w-0 flex-1 font-mono text-[13px] wrap-anywhere">
                      <span className="text-muted-foreground">{exchange.method}</span> {url.pathname}
                      {url.search}
                    </code>
                    <StatusBadge tone={exchange.status < 400 ? "success" : "danger"} className="tabular-nums">
                      {exchange.status}
                    </StatusBadge>
                  </summary>
                  <div className="pb-3 pl-8">
                    <CodeBlock
                      value={JSON.stringify({ request: exchange.request, response: exchange.response }, null, 2)}
                      label="request and response"
                    />
                  </div>
                </details>
              </li>
            );
          })}
        </ul>
      )}
    </Subsection>
  );
}

function stateLabel(state: Step["state"]): string {
  return { complete: "Done", current: "In progress", upcoming: "Pending", failed: "Failed" }[state];
}
