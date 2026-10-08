import { ChevronRight, type LucideIcon } from "lucide-react";
import { useState, useSyncExternalStore, type ComponentProps, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Hash } from "@/components/ui/hash";
import type { StatusTone } from "@/components/ui/status-badge";
import { cn } from "@/lib/utils";
import { ApiError } from "./api.js";
import { networkOf } from "./chains.js";
import { useNetworks } from "./queries.js";

/** An inline text link, in the page's text colour. */
export const LINK = "font-medium underline decoration-foreground/30 underline-offset-4 transition-colors hover:decoration-foreground";

/** The integration guide, where each tab's one-line explanation is told in full. */
export const INTEGRATION_GUIDE = "https://github.com/Phala-Network/phala-pay/blob/main/docs/integration.md";

/** On a phone, a control is 44px tall: a touch target's size. */
export const TOUCH = "max-sm:h-11";

/**
 * Every table of the backend: 44px rows under muted 12px headings, its first and last columns on
 * the panel's content edges.
 */
export const TABLE =
  "text-sm [&_th]:h-10 [&_th]:px-2 [&_th]:text-xs [&_th]:font-medium [&_th]:tracking-wider [&_th]:text-muted-foreground [&_th]:uppercase [&_td]:h-11 [&_td]:px-2 [&_td]:py-2 [&_tr>*:first-child]:pl-0 [&_tr>*:last-child]:pr-0";

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

/** Whether the media query matches, following it as it changes. The demo renders on the client only. */
export function useMediaQuery(query: string): boolean {
  return useSyncExternalStore(
    (onChange) => {
      const list = window.matchMedia(query);
      list.addEventListener("change", onChange);
      return () => list.removeEventListener("change", onChange);
    },
    () => window.matchMedia(query).matches,
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
 * Labelled values, one per row and each row at least 44px tall, as a table's: the label in the
 * muted colour beside its value; on a phone, above it.
 */
export function DataList({ className, ...props }: ComponentProps<"dl">) {
  return <dl className={cn("flex flex-col text-sm", className)} {...props} />;
}

export function DataItem({ label, className, ...props }: ComponentProps<"dd"> & { label: ReactNode }) {
  return (
    <div className="grid min-h-11 content-center gap-x-4 gap-y-0.5 py-2 sm:grid-cols-[minmax(0,10rem)_minmax(0,1fr)] sm:items-center">
      <dt className="text-muted-foreground">{label}</dt>
      <dd className={cn("min-w-0 wrap-anywhere", className)} {...props} />
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
    <section className={cn("flex min-w-0 flex-col gap-3", className)} aria-labelledby={id}>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <h4 id={id} className="text-sm font-semibold">
          {title}
        </h4>
        {aside}
      </div>
      {children}
    </section>
  );
}

/** Secondary detail, collapsed until opened. */
export function Disclosure({ summary, children }: { summary: ReactNode; children: ReactNode }) {
  return (
    <details className="group/disclosure">
      <summary className="flex min-h-11 w-fit cursor-pointer list-none items-center gap-1.5 rounded-sm text-sm font-medium [&::-webkit-details-marker]:hidden">
        <ChevronRight
          className="size-4 shrink-0 text-muted-foreground transition-transform group-open/disclosure:rotate-90 motion-reduce:transition-none"
          aria-hidden="true"
        />
        {summary}
      </summary>
      <div className="mt-1 flex flex-col gap-2">{children}</div>
    </details>
  );
}

/**
 * A long list's most recent `limit` items, and a "Show all (N)" button that shows the rest in the
 * page's flow (the page grows; nothing scrolls inside). `newestFirst` lists keep their first items;
 * the others (logs, oldest first) their last. With a `limit` of 0 the list is a disclosure, the
 * button named by `name`: "Show finalized sweeps (3)".
 */
export function useShowAll<T>(items: T[], limit: number, newestFirst = true, name = "all"): { shown: T[]; toggle: ReactNode } {
  const [all, setAll] = useState(false);
  const hidden = items.length - limit;
  const shown = all || hidden <= 0 ? items : newestFirst ? items.slice(0, limit) : items.slice(-limit);
  // The label lines up with the list above it; the button's hover fill reaches into the gutter.
  const toggle = hidden <= 0 ? null : (
    <Button type="button" variant="ghost" size="sm" className={cn("-ml-3 self-start", TOUCH)} aria-expanded={all} onClick={() => setAll((open) => !open)}>
      {all ? (limit === 0 ? `Hide ${name}` : "Show fewer") : `Show ${name} (${items.length})`}
    </Button>
  );
  return { shown, toggle };
}

/** A one-line explanation's link to the integration guide, which explains it in full. */
export function LearnMore({ anchor, topic }: { anchor: string; topic: string }) {
  return (
    <a className={cn(LINK, "whitespace-nowrap")} href={`${INTEGRATION_GUIDE}#${anchor}`} target="_blank" rel="noreferrer">
      Learn more<span className="sr-only"> about {topic}</span>
    </a>
  );
}

/** A part's message while it has nothing to show: one line of text. */
export function Empty({ children }: { children: ReactNode }) {
  return <p className="text-sm text-pretty text-muted-foreground">{children}</p>;
}

/**
 * A whole panel with nothing to show yet: centred in the space the panel has, its icon, what will
 * appear, and how to make it appear.
 */
export function EmptyState({ icon: Icon, title, children }: { icon: LucideIcon; title: string; children: ReactNode }) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-3 px-6 py-12 text-center">
      <span className="flex size-10 items-center justify-center rounded-full border bg-muted/60 text-muted-foreground">
        <Icon className="size-5" strokeWidth={1.75} aria-hidden="true" />
      </span>
      <div className="flex max-w-xs flex-col gap-1">
        <p className="text-sm font-medium">{title}</p>
        <p className="text-sm text-pretty text-muted-foreground">{children}</p>
      </div>
    </div>
  );
}

const TONES: Record<string, StatusTone> = {
  credited: "success",
  complete: "success",
  succeeded: "success",
  swept: "success",
  rejected: "danger",
  expired: "danger",
  canceled: "danger",
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
