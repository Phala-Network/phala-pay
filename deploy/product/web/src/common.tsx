import { ChevronRight, Info } from "lucide-react";
import type { ComponentProps, ReactNode } from "react";
import { Hash } from "@/components/ui/hash";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import type { StatusTone } from "@/components/ui/status-badge";
import { cn } from "@/lib/utils";
import { ApiError } from "./api.js";
import { networkOf } from "./chains.js";
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

/** GitHub's mark (./icons/github.svg), in the text colour. */
export function GitHubIcon({ className }: { className?: string }) {
  return (
    <span
      aria-hidden="true"
      className={cn("github-icon inline-block size-4 shrink-0 bg-current", className)}
    />
  );
}

/**
 * A transaction or address, as a `Hash` linked to its chain's explorer; with `copy`, a copy button
 * beside it.
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
  return (
    <Hash
      value={value}
      href={explorer === null ? undefined : `${explorer}/${kind}/${value}`}
      copyLabel={copy ? `Copy ${kind === "tx" ? "transaction hash" : "address"}` : undefined}
    />
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
      <dt className="font-sans text-muted-foreground">{label}</dt>
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

const TONES: Record<string, StatusTone> = {
  credited: "success",
  succeeded: "success",
  swept: "success",
  rejected: "danger",
  expired: "danger",
  reversed: "danger",
  failed: "danger",
};

/** A payment's or refund's status as a `StatusBadge` tone: success, failure, or else neutral. */
export function statusTone(status: string): StatusTone {
  return TONES[status] ?? "neutral";
}

export function describe(error: unknown): string {
  if (error instanceof ApiError) {
    return error.code === "rate_limited" ? "too many requests, try again in a minute" : error.code;
  }
  return "network error";
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
