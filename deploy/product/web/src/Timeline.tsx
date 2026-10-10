import { Check, ChevronDown, ChevronRight, X } from "lucide-react";
import { useId, useState } from "react";
import { CodeBlock } from "@/components/ui/code-block";
import { Hash } from "@/components/ui/hash";
import { StatusBadge } from "@/components/ui/status-badge";
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
import { DataItem, DataList, Empty, ExplorerLink, LearnMore, Subsection, useShowAll } from "./common.js";
import { assetOf, networkOf } from "./chains.js";
import { approx, clock, dollars, duration, rate, signedDollars, time, tokens } from "./format.js";
import { QueryState, type QueryView } from "./queryView.js";
import { useNetworks } from "./queries.js";

// Each step's title, what it waits for, and the time it usually takes: the hints are expectations,
// every time shown next to them is real (the chain's block time, the service's timestamps, or when
// your server received a webhook).
// A step that usually takes a known time says so on its line until it happens (`usually`); the
// credit's comes from the chain's `typical_credit_seconds` (`GET /v1/config`).
// `short` names a step in the stepper's narrow columns from lg.
const STEP_COPY: Record<StepKey, { title: string; short: string; hint: string; failed?: string; usually?: string }> = {
  quote_created: {
    short: "Quote",
    title: "Quote created",
    hint: "A locked price for an exact amount, for 15 minutes.",
  },
  sent: {
    short: "Sent",
    title: "Sent on chain",
    hint: "Waiting for a transfer to the address.",
    failed: "The quote expired without a payment.",
  },
  received: {
    short: "Received",
    title: "Received by Phala Pay",
    hint:
      "Usually within a block of sending, about 12 s on Sepolia and 2 s on Base Sepolia: the " +
      "service scans every new block for its addresses.",
  },
  credited: {
    short: "Credited",
    title: "Credited",
    hint:
      "At the chain's confirmation: 2 blocks on Sepolia, about 30 s after sending, and 3 blocks on " +
      "Base Sepolia, about 7 s. Once both RPC providers report the same block, the deposit is " +
      "valued and screened.",
    failed: "The deposit was rejected and will not be credited.",
  },
  webhook_received: {
    short: "Webhook",
    title: "Webhook handled by your server",
    hint: "The signed deposit.credited moves the balance; its metadata arrives with it.",
  },
  final: {
    short: "Final",
    usually: "~15 min",
    title: "Final",
    hint:
      "About 15 minutes on Ethereum mainnet; this demo runs on testnets. Until then a reorg that replaces the transaction reverses the " +
      "credit; only a final deposit can be refunded. The time shown is the deposit's final_at.",
  },
  reversed: {
    short: "Reversed",
    title: "Reversed",
    hint: "The transaction left the chain before finality; deposit.reversed took the credit back.",
    failed: "The transaction left the chain before finality; deposit.reversed took the credit back.",
  },
  swept: {
    short: "Swept",
    title: "Swept to the treasury",
    hint:
      "Phala Pay never sweeps: the merchant signs factory.flush from its own wallet or Safe (see " +
      "Sweeps below). The deposit is marked swept once that flush is finalized.",
  },
};

// A quote's steps, shown before there is a payment to follow.
const PREVIEW: StepKey[] = ["quote_created", "sent", "received", "credited", "webhook_received", "final", "swept"];

/**
 * The payment's steps as a stepper: a row of steps where its container is 32rem wide or more (a
 * column each: its dot, its name, its time), else a list, so the names never crowd. Each step opens to what it waits for and its data, shown under the row,
 * so the row stays one line high and the backend's tabs keep their room. Times are real: the time
 * since sending for the steps after it, else the time it happened; for a step yet to happen, how
 * long it usually takes.
 */
export function EventStream({ timeline: timelineView, loading }: { timeline: QueryView<Timeline>; loading: string | null }) {
  const timeline = timelineView.data ?? null;
  const networks = useNetworks().data;
  const id = useId();
  const [open, setOpen] = useState<StepKey[]>([]);
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
  if (loading !== null && timeline === null) {
    if (timelineView.error !== null) {
      return <QueryState view={timelineView} />;
    }
    return (
      <ol className={STEPPER} aria-label={`Loading ${loading}`} aria-busy="true">
        {PREVIEW.map((key) => (
          <li key={key} className="flex h-11 items-center gap-3 px-2 @lg:h-auto @lg:flex-col @lg:gap-2 @lg:py-2">
            <span className="size-3 rounded-full bg-muted motion-safe:animate-pulse" />
            <span className="h-2.5 w-24 rounded-full bg-muted motion-safe:animate-pulse @lg:w-12" />
          </li>
        ))}
      </ol>
    );
  }
  const steps: Step[] = timeline?.steps ?? PREVIEW.map((key) => ({ key, state: "upcoming", at: null, details: [] }));
  const sent = timeline?.sent?.at ?? null;
  const toggle = (key: StepKey) => setOpen((keys) => (keys.includes(key) ? keys.filter((each) => each !== key) : [...keys, key]));
  const failed = steps.find((step) => step.state === "failed" && STEP_COPY[step.key].failed !== undefined);
  return (
    <div className="flex flex-col gap-2">
      <ol className={STEPPER} aria-label={timeline === null ? "The steps of a payment" : "Payment timeline"}>
        {steps.map((step, index) => (
          <StreamStep
            key={step.key}
            step={step}
            sent={sent}
            // The current step shows when it started: when the step before it completed.
            since={
              step.state === "current" && step.at === null
                ? Math.max(0, ...steps.slice(0, index).map((each) => each.at ?? 0)) || null
                : null
            }
            token={token}
            open={open.includes(step.key)}
            details={`${id}-${step.key}`}
            onToggle={() => toggle(step.key)}
          />
        ))}
      </ol>
      {failed !== undefined && <p className="text-sm text-pretty text-destructive">{STEP_COPY[failed.key].failed}</p>}
      {steps.filter((step) => open.includes(step.key)).map((step) => (
        <StepDetails key={step.key} id={`${id}-${step.key}`} step={step} sent={sent} token={token} />
      ))}
    </div>
  );
}

/** The steps: a list in a narrow container, from 32rem a row of equal columns, however many steps there are. */
const STEPPER = "flex flex-col @lg:grid @lg:auto-cols-fr @lg:grid-flow-col";

function StepDot({ state }: { state: Step["state"] }) {
  if (state === "current") {
    return (
      <span className="relative z-10 flex size-4 items-center justify-center" aria-hidden="true">
        <span className="size-2.5 rounded-full bg-brand ring-2 ring-foreground/20" />
      </span>
    );
  }
  return (
    <span
      className={cn(
        "relative z-10 flex size-4 items-center justify-center rounded-full bg-card transition-colors motion-reduce:transition-none",
        state === "complete" && "bg-brand-ink text-background",
        state === "failed" && "bg-destructive text-background",
        state === "upcoming" && "border border-input",
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

/** Seconds since the payment was sent, for the steps after it. */
function elapsedOf(step: Step, sent: number | null): number | null {
  return step.at !== null && sent !== null && step.key !== "sent" && step.key !== "quote_created" ? step.at - sent : null;
}

/**
 * A step's button. Its rail (in the brand's colour once the step is complete, as the money has
 * passed it) runs to the next step's dot through the dots' centres: down the list on
 * a phone (from below this dot to above the next, each line 44px with its dot centred), across the
 * row from lg (from half a dot and a gap past this column's centre to as far before the next's).
 */
function StreamStep({
  step,
  sent,
  since = null,
  token,
  open,
  details,
  onToggle,
}: {
  step: Step;
  sent: number | null;
  since?: number | null;
  token: StepToken;
  open: boolean;
  details: string;
  onToggle: () => void;
}) {
  const copy = STEP_COPY[step.key];
  const usually =
    step.key === "credited" ? (token.creditSeconds === null ? undefined : approx(token.creditSeconds)) : copy.usually;
  const elapsed = elapsedOf(step, sent);
  return (
    <li
      className="relative before:absolute before:top-7.5 before:-bottom-3.5 before:left-4 before:w-px before:-translate-x-1/2 before:bg-border last:before:hidden @lg:before:top-4 @lg:before:right-[calc(-50%+0.75rem)] @lg:before:bottom-auto @lg:before:left-[calc(50%+0.75rem)] @lg:before:h-px @lg:before:w-auto @lg:before:translate-x-0 data-[state=complete]:before:bg-brand-ink"
      data-step={step.key}
      data-state={step.state}
      aria-current={step.state === "current" ? "step" : undefined}
    >
      <button
        type="button"
        aria-expanded={open}
        aria-controls={open ? details : undefined}
        onClick={onToggle}
        className="group/trigger grid min-h-11 w-full grid-cols-[1rem_minmax(0,1fr)_auto] items-center gap-x-3 rounded-md px-2 py-1.5 text-left transition-colors hover:bg-muted/60 aria-expanded:bg-muted/60 motion-reduce:transition-none @lg:flex @lg:flex-col @lg:items-center @lg:gap-1 @lg:px-1 @lg:py-2 @lg:text-center"
      >
        <StepDot state={step.state} />
        <span className="flex min-w-0 items-center gap-1.5">
          <span
            className={cn(
              "min-w-0 text-sm text-pretty",
              step.state === "upcoming" && "text-muted-foreground",
              (step.state === "current" || open) && "font-medium",
              step.state === "failed" && "text-destructive",
            )}
          >
            <span className="@lg:hidden">{copy.title}</span>
            <span className="hidden @lg:inline">{copy.short}</span>
            <span className="sr-only">, {stateLabel(step.state)}</span>
          </span>
          <ChevronDown
            className="size-4 shrink-0 text-muted-foreground transition-transform group-aria-expanded/trigger:rotate-180 motion-reduce:transition-none @lg:hidden"
            aria-hidden="true"
          />
        </span>
        <span className="text-right text-sm whitespace-nowrap text-muted-foreground tabular-nums @lg:text-center @lg:text-sm/4">
          {elapsed !== null ? (
            `+${duration(elapsed)}`
          ) : step.at !== null ? (
            <time dateTime={new Date(step.at * 1000).toISOString()}>{clock(step.at)}</time>
          ) : (step.state === "upcoming" || step.state === "current") && usually !== undefined ? (
            <><span className="@lg:hidden">usually </span>{usually}</>
          ) : since !== null ? (
            <time dateTime={new Date(since * 1000).toISOString()}><span className="@lg:hidden">since </span>{clock(since)}</time>
          ) : null}
        </span>
      </button>
    </li>
  );
}

/** An opened step: what it waits for, when it happened, and its data. */
function StepDetails({ id, step, sent, token }: { id: string; step: Step; sent: number | null; token: StepToken }) {
  const copy = STEP_COPY[step.key];
  const elapsed = elapsedOf(step, sent);
  return (
    <section id={id} aria-label={`${copy.title}: details`} data-step-details={step.key}
      className="flex flex-col gap-2 border-l-2 py-1 pl-3 text-sm">
      <h5 className="font-medium">{copy.title}</h5>
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
    </section>
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

/**
 * The deposit's ledger as one equation, as every `deposit.*` event's cumulative amounts give it,
 * then, on a line of its own, what the product's server holds for it.
 */
export function LedgerPanel({ ledger }: { ledger: LedgerView }) {
  const product = ledger.product;
  return (
    <Subsection title="Ledger" id="ledger-title">
      <p data-testid="ledger" className="text-sm tabular-nums">
        <span className="text-muted-foreground">{ledger.status}: </span>
        {ledger.amount === null ? "—" : dollars(ledger.amount)}
        <span className="text-muted-foreground"> − {dollars(ledger.amount_refunded)} refunded − {dollars(ledger.amount_reversed)} reversed = </span>
        <span className="font-semibold" data-testid="nets-to">{dollars(ledger.nets_to)}</span>{" "}
        <LearnMore anchor="the-balance-rule-and-event-ordering" topic="the balance rule" />
      </p>
      <p className="text-sm text-muted-foreground tabular-nums">
        Your server's ledger:{" "}
        <span className="text-foreground" data-testid="console-net">
          {product === null || product.status === null
            ? "no order yet"
            : product.net === null
              ? `${product.status}${product.reason === null ? "" : ` (${product.reason})`}`
              : `${dollars(product.net)} (credit ${dollars(product.credit ?? 0)}${product.adjustments
                  .map((adjustment) => `, ${signedDollars(adjustment.amount)} by ${adjustment.reason}`)
                  .join("")})`}
        </span>
        {product?.bonus != null && product.bonus !== 0 && (
          <>
            ; this demo's bonus <span className="text-foreground" data-testid="console-bonus">{signedDollars(product.bonus)}</span>
          </>
        )}
      </p>
    </Subsection>
  );
}

/**
 * The webhook events the product's server received, newest last: a row each, like the requests
 * below it, with its type, when it arrived, and whether its signature verified.
 */
export function EventsLog({ events }: { events: WebhookEvent[] }) {
  // The latest events (the log runs oldest first); the rest on request.
  const recent = useShowAll(events, 2, false);
  return (
    <Subsection title="Webhook events received" id="events-title">
      {events.length === 0 ? (
        <Empty>None yet.</Empty>
      ) : (
        <ul className="flex flex-col divide-y border-y text-sm">
          {recent.shown.map((event) => (
            <li key={event.id} data-testid="webhook-event" className="flex min-h-11 items-center gap-3 px-2 py-2">
              <code className="min-w-0 flex-1 font-mono text-mono wrap-anywhere">{event.type}</code>
              <span className="text-muted-foreground tabular-nums" title={time(event.received_at)}>
                {clock(event.received_at)}
              </span>
              {event.verified ? (
                <StatusBadge tone="success">Verified</StatusBadge>
              ) : (
                <StatusBadge tone="neutral">Not verified</StatusBadge>
              )}
            </li>
          ))}
        </ul>
      )}
      {recent.toggle}
    </Subsection>
  );
}

/** The product's API requests: sent from its server with its restricted key, never the browser. */
export function Requests({ exchanges, title, id }: { exchanges: ApiExchange[]; title: string; id: string }) {
  // The latest requests (oldest first); the rest on request.
  const recent = useShowAll(exchanges, 2, false);
  return (
    <Subsection title={`${title} (${exchanges.length})`} id={id}>
      <p className="text-sm text-pretty text-muted-foreground">
        Sent by the product's server with its restricted key; the browser never holds it.
      </p>
      {exchanges.length === 0 ? (
        <Empty>None yet.</Empty>
      ) : (
        <ul className="flex flex-col divide-y border-y">
          {recent.shown.map((exchange, index) => {
            const url = new URL(exchange.url);
            return (
              <li key={`${exchange.method}-${exchange.url}-${index}`}>
                <details className="group/exchange">
                  <summary className="flex min-h-11 cursor-pointer list-none items-center gap-2 px-2 py-2 transition-colors hover:bg-muted/60 [&::-webkit-details-marker]:hidden">
                    <ChevronRight
                      className="size-4 shrink-0 text-muted-foreground transition-transform group-open/exchange:rotate-90 motion-reduce:transition-none"
                      aria-hidden="true"
                    />
                    {/* The method and path; the query, with the request and response, once opened. */}
                    <code className="min-w-0 flex-1 font-mono text-mono wrap-anywhere">
                      <span className="text-muted-foreground">{exchange.method}</span> {url.pathname}
                      {url.search !== "" && <span className="text-muted-foreground"> ?…</span>}
                    </code>
                    <StatusBadge tone={exchange.status < 400 ? "success" : "danger"} className="tabular-nums">
                      {exchange.status}
                    </StatusBadge>
                  </summary>
                  <div className="flex flex-col gap-2 pb-3 pl-8">
                    <code className="font-mono text-mono wrap-anywhere">{exchange.method} {url.pathname}{url.search}</code>
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
      {recent.toggle}
    </Subsection>
  );
}

function stateLabel(state: Step["state"]): string {
  return { complete: "Done", current: "In progress", upcoming: "Pending", failed: "Failed" }[state];
}
