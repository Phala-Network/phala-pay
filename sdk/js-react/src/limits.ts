import { formatUnitsGrouped } from "@phala/pay";

/**
 * A token's per-transfer limits: a deposit address's asset, or a `GET /v1/config` asset, which
 * carries the same terms. Any field may be absent, as on older servers.
 */
export interface DepositLimitsSource {
  asset: string;
  decimals?: unknown;
  min_amount?: unknown;
  min_deposit_atomic?: unknown;
  max_deposit_atomic?: unknown;
}

/**
 * The token's per-transfer limits as text, for example `at least $5.00 and up to 10,100 USDC per
 * transfer`. Both independent floors apply; never interpret missing or malformed limits as limits.
 */
export function depositLimits(token: DepositLimitsSource): string | undefined {
  const { min_amount: creditMinimum, min_deposit_atomic: minimum, max_deposit_atomic: maximum, decimals } = token;
  if (
    typeof creditMinimum !== "number" || !Number.isSafeInteger(creditMinimum) || creditMinimum <= 0 ||
    typeof minimum !== "string" || minimum.length === 0 || /[^0-9]/.test(minimum) ||
    typeof maximum !== "string" || maximum.length === 0 || /[^0-9]/.test(maximum) ||
    typeof decimals !== "number" || !Number.isInteger(decimals) || decimals < 0 || decimals > 255
  ) {
    return undefined;
  }
  const minAtomic = BigInt(minimum);
  const maxAtomic = BigInt(maximum);
  if (minAtomic > maxAtomic) return undefined;
  const credit = new Intl.NumberFormat("en", { style: "currency", currency: "USD" }).format(creditMinimum / 100);
  const symbol = token.asset.toUpperCase();
  const upper = `${formatUnitsGrouped(maxAtomic, decimals, "en")} ${symbol} per transfer`;
  return minAtomic === 0n
    ? `at least ${credit} and up to ${upper}`
    : `at least ${credit} and ${formatUnitsGrouped(minAtomic, decimals, "en")} ${symbol}, up to ${upper}`;
}
