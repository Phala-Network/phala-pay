import { Check, ChevronDown, ChevronRight, X } from "lucide-react";
import { CodeBlock } from "@/components/ui/code-block";
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
import { Detail, Details, Empty, ExplorerLink, InfoTip, Subsection } from "./common.js";
import { assetOf, networkOf } from "./chains.js";
import { approx, clock, dollars, duration, rate, short, signedDollars, time, tokens } from "./format.js";
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

// A step's line in columns: its time, its dot, its title with the opener, and the time since
// sending, which ends on the card's right content edge. The time's column is kept before a payment
// has times, so the lines stay put when its first one arrives.
const STEP_GRID = "grid grid-cols-[4rem_1rem_minmax(0,1fr)_auto] items-center gap-x-2 sm:gap-x-3";
// Where a step's opened details start: under its title (px-2, 4rem, 1rem, and two gaps); on a
// phone, under its time, to keep the details' width.
const STEP_INDENT = "pl-2 sm:pl-[calc(0.5rem+5rem+1.5rem)]";

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
        <p className="px-2 text-sm text-pretty text-muted-foreground">
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
      return <QueryState view={timelineView} className="px-2 text-sm" />;
    }
    return (
      <ol className="flex flex-col" aria-label={`Loading ${short(loading)}`} aria-busy="true">
        {PREVIEW.map((key) => (
          <li key={key} className={cn(STEP_GRID, "h-9 px-2")}>
            <span className="h-2 w-12 rounded-full bg-muted motion-safe:animate-pulse" />
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
      <span className="relative flex size-4 items-center justify-center" aria-hidden="true">
        <span className="size-2.5 rounded-full bg-brand ring-2 ring-primary/30" />
      </span>
    );
  }
  return (
    <span
      className={cn(
        "flex size-4 items-center justify-center rounded-full transition-colors duration-500 motion-reduce:transition-none",
        state === "complete" && "bg-foreground/90 text-background",
        state === "failed" && "bg-destructive text-background",
        state === "upcoming" && "border border-dashed border-foreground/25",
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
    <li
      className="group/step relative"
      data-step={step.key}
      data-state={step.state}
      aria-current={step.state === "current" ? "step" : undefined}
    >
      {/* The rail between this step's dot and the next one's, through the dots' centres (px-2, the
          time's column, a gap, half a dot); on a phone the opened details take its place. */}
      <span
        className="absolute top-7 bottom-[-0.375rem] left-[calc(5.5rem-0.5px)] w-px bg-border group-last/step:hidden max-sm:group-has-[[data-state=open]]/step:hidden sm:left-[calc(5.75rem-0.5px)]"
        aria-hidden="true"
      />
      <Collapsible>
        <CollapsibleTrigger
          className={cn(
            STEP_GRID,
            "group/trigger relative min-h-9 w-full rounded-md px-2 py-1.5 text-left focus-visible:outline-hidden transition-colors hover:bg-card dark:hover:bg-muted focus-visible:ring-2 focus-visible:ring-ring motion-reduce:transition-none",
          )}
        >
          {/* A step yet to happen leaves its time blank. */}
          <span className="font-mono text-xs text-muted-foreground tabular-nums">
            {step.at !== null ? (
              <time dateTime={new Date(step.at * 1000).toISOString()}>{clock(step.at)}</time>
            ) : since !== null ? (
              <time dateTime={new Date(since * 1000).toISOString()} title="Waiting since">
                {clock(since)}
              </time>
            ) : null}
          </span>
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
              className="size-3.5 shrink-0 text-muted-foreground opacity-0 transition-[transform,opacity] group-hover/step:opacity-100 group-focus-within/step:opacity-100 group-data-[state=open]/trigger:rotate-180 group-data-[state=open]/trigger:opacity-100 motion-reduce:transition-none"
              aria-hidden="true"
            />
          </span>
          <span className="text-right font-mono text-xs whitespace-nowrap text-muted-foreground tabular-nums">
            {elapsed !== null ? (
              `+${duration(elapsed)}`
            ) : (step.state === "upcoming" || step.state === "current") && usually !== undefined ? (
              <>
                <span className="font-sans">usually </span>
                {usually}
              </>
            ) : null}
          </span>
        </CollapsibleTrigger>
        {failed && (
          <p className={cn("pb-1 text-xs text-pretty text-destructive", STEP_INDENT)}>
            {copy.failed}
          </p>
        )}
        <CollapsibleContent
          className={cn("flex flex-col gap-2 pr-2 pb-3 text-xs", STEP_INDENT)}
        >
          <p className="text-muted-foreground text-pretty">{copy.hint}</p>
          {step.at !== null && (
            <p className="font-mono text-xs" data-testid="step-time">
              {time(step.at)}
              {elapsed !== null && <span className="text-muted-foreground"> · {duration(elapsed)} after sending</span>}
            </p>
          )}
          {step.details.length > 0 && (
            <Details>
              {step.details.map((detail) => (
                <Detail key={detail.label} label={detail.label}>
                  <DetailValue detail={detail} token={token} />
                </Detail>
              ))}
            </Details>
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
  if ((kind === "address" || kind === "tx") && typeof value === "string") {
    return <ExplorerLink chainId={token.chainId} kind={kind} value={value} />;
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
  return <span className={detail.mono === true ? "font-mono" : undefined}>{String(value)}</span>;
}

export function LedgerPanel({ ledger }: { ledger: LedgerView }) {
  const product = ledger.product;
  return (
    <Subsection
      title="Ledger"
      id="ledger-title"
      aside={
        <InfoTip label="About the balance rule">
          Every deposit.* event carries those cumulative amounts, so the result does not depend on the order events
          arrive in.
        </InfoTip>
      }
    >
      <p className="text-muted-foreground">
        While <code>credited</code> or <code>reversed</code>, a deposit nets to{" "}
        <code>amount − amount_refunded − amount_reversed</code>; otherwise to 0.
      </p>
      <Details data-testid="ledger" className="font-mono tabular-nums">
        <Detail label="Status">{ledger.status}</Detail>
        <Detail label="amount">{ledger.amount === null ? "—" : dollars(ledger.amount)}</Detail>
        <Detail label="amount_refunded">−{dollars(ledger.amount_refunded)}</Detail>
        <Detail label="amount_reversed">−{dollars(ledger.amount_reversed)}</Detail>
        <Detail label="Nets to" data-testid="nets-to">
          <strong>{dollars(ledger.nets_to)}</strong>
        </Detail>
        <Detail label="Your server's ledger" data-testid="console-net">
          {product === null || product.status === null
            ? "no order yet"
            : product.net === null
              ? `${product.status}${product.reason === null ? "" : ` (${product.reason})`}`
              : `${dollars(product.net)} (credit ${dollars(product.credit ?? 0)}${product.adjustments
                  .map((adjustment) => `, ${signedDollars(adjustment.amount)} by ${adjustment.reason}`)
                  .join("")})`}
        </Detail>
        {product?.bonus != null && product.bonus !== 0 && (
          <Detail label="Bonus (this demo's)" data-testid="console-bonus">
            {signedDollars(product.bonus)}, its promotion's share of what the credit nets to
          </Detail>
        )}
      </Details>
    </Subsection>
  );
}

export function EventsLog({ events }: { events: WebhookEvent[] }) {
  return (
    <Subsection title="Webhook events received" id="events-title">
      {events.length === 0 ? (
        <Empty>None yet.</Empty>
      ) : (
        <Table className="text-xs">
          <TableHeader>
            <TableRow>
              <TableHead scope="col">Type</TableHead>
              <TableHead scope="col">Event id</TableHead>
              <TableHead scope="col">Received</TableHead>
              <TableHead scope="col">Signature</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {events.map((event) => (
              <TableRow key={event.id} data-testid="webhook-event">
                <TableCell className="font-mono text-muted-foreground">{event.type}</TableCell>
                <TableCell className="font-mono text-muted-foreground" title={event.id}>
                  {short(event.id)}
                </TableCell>
                <TableCell className="font-mono tabular-nums">{time(event.received_at)}</TableCell>
                <TableCell className="text-success">{event.verified ? "Verified" : "—"}</TableCell>
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
    <Subsection
      title={`${title} (${exchanges.length})`}
      id={id}
      aside={
        <InfoTip label="About these requests">
          Sent from the product's server with its restricted API key; the browser never holds it.
        </InfoTip>
      }
    >
      {exchanges.length === 0 ? (
        <Empty>None yet.</Empty>
      ) : (
        <ul className="flex flex-col divide-y rounded-lg border">
          {exchanges.map((exchange, index) => (
            <li key={`${exchange.method}-${exchange.url}-${index}`}>
              <details className="group/exchange">
                <summary className="flex cursor-pointer list-none items-center gap-2 px-3 py-2 font-mono focus-visible:outline-hidden hover:bg-card dark:hover:bg-muted focus-visible:ring-2 focus-visible:ring-ring [&::-webkit-details-marker]:hidden">
                  <ChevronRight
                    className="size-3.5 shrink-0 text-muted-foreground transition-transform group-open/exchange:rotate-90 motion-reduce:transition-none"
                    aria-hidden="true"
                  />
                  <code className="min-w-0 flex-1 wrap-anywhere">
                    <span className="text-muted-foreground">{exchange.method}</span> {new URL(exchange.url).pathname}
                    {new URL(exchange.url).search}
                  </code>
                  <span className={exchange.status < 400 ? "text-success" : "text-destructive"}>{exchange.status}</span>
                </summary>
                <CodeBlock
                  value={JSON.stringify({ request: exchange.request, response: exchange.response }, null, 2)}
                  label="request and response"
                  className="max-h-80 rounded-none border-x-0 border-b-0"
                />
              </details>
            </li>
          ))}
        </ul>
      )}
    </Subsection>
  );
}

function stateLabel(state: Step["state"]): string {
  return { complete: "Done", current: "In progress", upcoming: "Pending", failed: "Failed" }[state];
}
