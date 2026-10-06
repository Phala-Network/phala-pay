import { formatUnits, parseUnits } from "viem";

const usd = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });

export function dollars(cents: number): string {
  return usd.format(cents / 100);
}

/** A round amount without its cents, for a preset: `$20`; any other amount as `dollars`. */
export function presetDollars(cents: number): string {
  return cents % 100 === 0 ? usd.format(cents / 100).replace(/\.00$/, "") : dollars(cents);
}

/** An amount taken off: `−$5.00`, but `$0.00` when nothing was. */
export function minusDollars(cents: number): string {
  return cents === 0 ? dollars(0) : `−${dollars(cents)}`;
}

/** `+$20.00` or `−$2.50`. */
export function signedDollars(cents: number): string {
  return `${cents < 0 ? "−" : "+"}${usd.format(Math.abs(cents) / 100)}`;
}

/** The exact token amount, grouped and without trailing zeros: `1,273.9185 PHA`. */
export function tokens(atomic: string, symbol: string, decimals = 18): string {
  const [whole = "0", fraction] = formatUnits(BigInt(atomic), decimals).split(".");
  const grouped = BigInt(whole).toLocaleString("en-US");
  return `${fraction === undefined ? grouped : `${grouped}.${fraction}`} ${symbol}`;
}

/** A decimal amount typed by the visitor, in a token's atomic units; `null` unless it is above 0. */
export function atomicAmount(amount: string, decimals: number): bigint | null {
  try {
    const atomic = parseUnits(amount.trim(), decimals);
    return atomic > 0n ? atomic : null;
  } catch {
    return null;
  }
}

/**
 * The page's one truncation of a hash, an address, or an id: its first 6 and last 4 characters,
 * besides its type prefix (`0x`, `qt_`, …), `0x1a2b3c…7e8f`. Its full value goes beside it (a
 * tooltip or title).
 */
export function short(value: string): string {
  const prefix = /^(?:0x|[a-z]+_)/.exec(value)?.[0].length ?? 0;
  return value.length > prefix + 11 ? `${value.slice(0, prefix + 6)}…${value.slice(-4)}` : value;
}

/** A moment, `Oct 6, 13:05:05`: the 24-hour clock, as every time on the page. */
export function time(seconds: number): string {
  return new Date(seconds * 1000).toLocaleString("en-US", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hourCycle: "h23",
  });
}

/** A table's date, `Sep 28, 15:21`. */
export function day(seconds: number): string {
  return new Date(seconds * 1000).toLocaleString("en-US", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hourCycle: "h23",
  });
}

/** The time of day, `15:21:05`: a log's timestamp. */
export function clock(seconds: number): string {
  return new Date(seconds * 1000).toLocaleTimeString("en-GB", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

/** A typical wait, rounded: `~30 s`, or `~5 min` from 90 seconds. */
export function approx(seconds: number): string {
  return seconds < 90 ? `~${Math.round(seconds)}\u00a0s` : `~${Math.round(seconds / 60)}\u00a0min`;
}

/** `12 s`, `3 min 5 s`, or `1 h 2 min`, as the page's copy writes times. */
export function duration(seconds: number): string {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) {
    return `${s}\u00a0s`;
  }
  if (s < 3600) {
    return `${Math.floor(s / 60)}\u00a0min ${s % 60}\u00a0s`;
  }
  return `${Math.floor(s / 3600)}\u00a0h ${Math.floor((s % 3600) / 60)}\u00a0min`;
}

export function statusLabel(status: string): string {
  const labels: Record<string, string> = {
    awaiting_payment: "Awaiting payment",
    complete: "Complete",
    expired: "Expired",
    pending: "Pending",
    credited: "Credited",
    rejected: "Rejected",
    reversed: "Reversed",
    succeeded: "Succeeded",
    failed: "Failed",
    canceled: "Canceled",
  };
  return labels[status] ?? status;
}

// A rate to 4 significant digits, but never fewer than whole cents: `$0.06041`, `$0.25`, `$1.00`,
// `$1,234.57`.
const precise = new Intl.NumberFormat("en-US", {
  style: "currency",
  currency: "USD",
  maximumFractionDigits: 2,
  maximumSignificantDigits: 4,
  roundingPriority: "morePrecision",
});

/** A USD-per-token rate, as the service states it (8 decimals), to 4 significant digits. */
export function price(exchangeRate: string): string {
  const value = Number(exchangeRate);
  const fraction = precise.formatToParts(value).find((part) => part.type === "fraction")?.value.length ?? 0;
  return fraction < 2 ? usd.format(value) : precise.format(value);
}

/** `1 PHA = $0.06041`. */
export function rate(symbol: string, exchangeRate: string): string {
  return `1 ${symbol} = ${price(exchangeRate)}`;
}

/** Basis points as a percentage: `10%`, `2.5%`. */
export function percent(bps: number): string {
  return `${new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 }).format(bps / 100)}%`;
}

/** A token as the customer sees it: `Test PHA` on a testnet, so it is never taken for real money. */
export function tokenName(symbol: string, testnet: boolean): string {
  return testnet ? `Test ${symbol}` : symbol;
}
