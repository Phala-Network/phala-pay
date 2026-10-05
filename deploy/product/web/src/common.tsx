import { Check, ChevronRight, Copy, Info } from "lucide-react";
import { useState, type ComponentProps, type ReactNode } from "react";
import { Badge } from "@/components/ui/badge";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { ApiError, isTerminalApiError } from "./api.js";
import { networkOf } from "./chains.js";
import { short } from "./format.js";
import { useNetworks } from "./queries.js";

/** An inline text link, in the page's text colour. */
export const LINK = "font-medium underline decoration-foreground/30 underline-offset-4 transition-colors hover:decoration-foreground";

/**
 * The visitor's wallet helpers (./testTokens), loaded on first use: they carry the chain and wallet
 * libraries, which the page's first paint does not need.
 */
export function wallet() {
  return import("./testTokens.js");
}

/** The SDK's components, loaded when a payment starts: they carry the chain and wallet libraries. */
export function loadSdk() {
  return import("@phala/pay-react");
}

/**
 * A form's main action: the page's one primary (`bg-primary`), full width. Phala's lime is only an
 * accent: the current step, focus, and highlights.
 */
export const PRIMARY_BUTTON = "h-11 w-full rounded-lg font-semibold";

/** GitHub's mark (./icons/github.svg), in the text colour. */
export function GitHubIcon({ className }: { className?: string }) {
  return (
    <span
      aria-hidden="true"
      className={cn("github-icon inline-block size-4 shrink-0 bg-current", className)}
    />
  );
}

/** Copies `value`, confirming with a tick for a moment. */
export function CopyButton({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      aria-label={copied ? "Copied" : label}
      className="inline-flex size-5 shrink-0 items-center justify-center rounded text-muted-foreground transition-colors outline-none hover:bg-muted hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring"
      onClick={() => {
        navigator.clipboard.writeText(value).then(
          () => {
            setCopied(true);
            setTimeout(() => setCopied(false), 1500);
          },
          () => undefined,
        );
      }}
    >
      {copied ? <Check className="size-3" aria-hidden="true" /> : <Copy className="size-3" aria-hidden="true" />}
    </button>
  );
}

/**
 * A transaction or address, middle-truncated (`short`), linked to its chain's explorer, with the
 * full value in a tooltip; with `copy`, a copy button beside it.
 */
export function ExplorerLink({
  chainId,
  kind,
  value,
  copy = false,
}: {
  chainId: number | undefined;
  kind: "address" | "tx";
  value: string;
  copy?: boolean;
}) {
  const explorer = networkOf(useNetworks().data, chainId)?.explorer ?? null;
  const link = (
    <Tooltip>
      <TooltipTrigger asChild>
        {explorer === null ? (
          <span className="font-mono text-xs">{short(value)}</span>
        ) : (
          <a className={cn("font-mono text-xs", LINK)} href={`${explorer}/${kind}/${value}`} target="_blank" rel="noreferrer">
            {short(value)}
          </a>
        )}
      </TooltipTrigger>
      <TooltipContent className="font-mono break-all">{value}</TooltipContent>
    </Tooltip>
  );
  return copy ? (
    <span className="inline-flex items-center gap-1">
      {link}
      <CopyButton value={value} label={`Copy ${kind === "tx" ? "transaction hash" : "address"}`} />
    </span>
  ) : (
    link
  );
}

/**
 * An explanation behind a small info icon: the page shows one short line, the popover the rest. It
 * opens on a click or a tap, so touch screens reach it too.
 */
export function InfoTip({ label, children, className }: { label: string; children: ReactNode; className?: string }) {
  return (
    <Popover>
      <PopoverTrigger asChild>
        <button
          type="button"
          aria-label={label}
          className={cn(
            "inline-flex size-4 shrink-0 items-center justify-center rounded-full text-muted-foreground transition-colors outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring data-[state=open]:text-foreground",
            className,
          )}
        >
          <Info className="size-3.5" aria-hidden="true" />
        </button>
      </PopoverTrigger>
      <PopoverContent side="bottom" collisionPadding={12} className="w-72 p-3 text-xs leading-relaxed text-pretty text-muted-foreground">
        {children}
      </PopoverContent>
    </Popover>
  );
}

/** A list of labelled values, such as a timeline step's details. */
export function Details({ className, ...props }: ComponentProps<"dl">) {
  // The card on the light console's tinted surface; muted on the dark one.
  return <dl className={cn("grid gap-1.5 rounded-lg bg-card p-3 text-xs dark:bg-muted", className)} {...props} />;
}

export function Detail({ label, className, ...props }: ComponentProps<"dd"> & { label: ReactNode }) {
  return (
    <div className="grid grid-cols-[minmax(0,7rem)_minmax(0,1fr)] gap-3 sm:grid-cols-[minmax(0,9rem)_minmax(0,1fr)]">
      <dt className="text-muted-foreground">{label}</dt>
      <dd className={cn("wrap-anywhere", className)} {...props} />
    </div>
  );
}

/** A titled part of a panel. */
export function Subsection({
  title,
  id,
  aside,
  children,
  className,
}: {
  title: string;
  id: string;
  aside?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={cn("flex min-w-0 flex-col gap-3 text-xs", className)} aria-labelledby={id}>
      <div className="flex items-center gap-2">
        <h3 id={id} className="text-sm font-medium">
          {title}
        </h3>
        {aside}
      </div>
      {children}
    </section>
  );
}

/** Secondary detail, collapsed until opened. */
export function Disclosure({ summary, children }: { summary: ReactNode; children: ReactNode }) {
  return (
    <details className="group/disclosure text-xs">
      <summary className="flex w-fit cursor-pointer list-none items-center gap-1 rounded-sm text-sm font-medium outline-none focus-visible:ring-2 focus-visible:ring-ring [&::-webkit-details-marker]:hidden">
        <ChevronRight
          className="size-3.5 shrink-0 text-muted-foreground transition-transform group-open/disclosure:rotate-90 motion-reduce:transition-none"
          aria-hidden="true"
        />
        {summary}
      </summary>
      <div className="mt-3 flex flex-col gap-2">{children}</div>
    </details>
  );
}

/** A panel's message while it has nothing to show: one line of text, one per panel. */
export function Empty({ children }: { children: ReactNode }) {
  return <p className="text-sm text-pretty text-muted-foreground">{children}</p>;
}

const TONES: Record<string, "success" | "danger"> = {
  credited: "success",
  succeeded: "success",
  swept: "success",
  rejected: "danger",
  expired: "danger",
  reversed: "danger",
  failed: "danger",
};

/** A payment's or refund's status: success and failure in their colours, anything else neutral. */
export function StatusBadge({ status, children }: { status: string; children: ReactNode }) {
  const tone = TONES[status];
  return (
    <Badge
      variant={tone === "danger" ? "destructive" : "secondary"}
      className={tone === "success" ? "bg-success/15 text-success" : undefined}
    >
      {children}
    </Badge>
  );
}

export function describe(error: unknown): string {
  if (error instanceof ApiError) {
    return error.code === "rate_limited" ? "too many requests, try again in a minute" : error.code;
  }
  return "network error";
}

/** A query failure shown only before there is data; never expose the API's internal error. */
export function queryErrorMessage(error: unknown, subject: string): string {
  return isTerminalApiError(error)
    ? `${subject} unavailable for this request.`
    : `${subject} unavailable right now; retrying…`;
}

export function errorMessage(error: unknown, fallback: string): string {
  const message = error instanceof Error ? error.message.split("\n")[0] : undefined;
  return message ?? fallback;
}

/**
 * Whether the wallet held too little for a payment, which was not sent: the SDK's `WalletError`
 * `insufficient_balance`, read by its code so the SDK stays out of the page's first load.
 */
export function isShortOfTokens(error: unknown): boolean {
  return error instanceof Error && "code" in error && error.code === "insufficient_balance";
}

/** Triggers a download of `value` as a JSON file (a Blob URL: no request leaves the page). */
export function downloadJson(name: string, value: unknown): void {
  const url = URL.createObjectURL(new Blob([JSON.stringify(value, null, 2)], { type: "application/json" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
