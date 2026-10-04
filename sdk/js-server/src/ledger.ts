import { LedgerSnapshotError } from "./errors.js";
import { isRecord } from "./json.js";
import type { components } from "./generated/openapi.js";
export type LedgerSnapshot = Readonly<
  Required<
    Pick<
      components["schemas"]["Deposit"],
      | "id"
      | "livemode"
      | "client_reference_id"
      | "currency"
      | "status"
      | "amount"
      | "amount_refunded"
      | "amount_reversed"
    >
  >
>;
const fail = (): never => {
  throw new LedgerSnapshotError("Invalid or conflicting deposit snapshot");
};
function snapshot(input: unknown): LedgerSnapshot {
  if (!isRecord(input)) return fail();
  const {
    id,
    livemode,
    client_reference_id,
    currency,
    status,
    amount,
    amount_refunded,
    amount_reversed,
  } = input;
  if (
    typeof id !== "string" ||
    !id ||
    typeof livemode !== "boolean" ||
    typeof client_reference_id !== "string" ||
    !client_reference_id ||
    typeof currency !== "string" ||
    typeof status !== "string" ||
    !["pending", "credited", "rejected", "reversed"].includes(status)
  )
    return fail();
  const minorUnits = (value: unknown): value is number =>
    typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
  if (
    !(amount === null || minorUnits(amount)) ||
    !minorUnits(amount_refunded) ||
    !minorUnits(amount_reversed)
  )
    return fail();
  if (
    (amount === null && (amount_refunded !== 0 || amount_reversed !== 0)) ||
    (amount !== null && amount_refunded + amount_reversed > amount) ||
    (amount_refunded > 0 && amount_reversed > 0) ||
    (status === "credited" && amount === null) ||
    (status === "reversed" && amount !== null && amount_reversed !== amount) ||
    (status !== "reversed" && amount_reversed !== 0)
  )
    return fail();
  return Object.freeze({
    id,
    livemode,
    client_reference_id,
    currency,
    status,
    amount,
    amount_refunded,
    amount_reversed,
  });
}
export function depositNetAmount(deposit: LedgerSnapshot): number {
  const value = snapshot(deposit);
  return ["credited", "reversed"].includes(value.status)
    ? (value.amount ?? 0) - value.amount_refunded - value.amount_reversed
    : 0;
}
export function balanceDelta(
  previous: LedgerSnapshot | null,
  deposit: LedgerSnapshot,
): Readonly<{ snapshot: LedgerSnapshot; contribution: number; delta: number }> {
  const next = snapshot(deposit);
  let merged = next;
  if (previous !== null) {
    const prior = snapshot(previous);
    for (const field of ["id", "livemode", "client_reference_id"] as const)
      if (prior[field] !== next[field]) fail();
    for (const field of ["currency", "amount"] as const)
      if (prior[field] !== null && next[field] !== null && prior[field] !== next[field]) fail();
    if (
      (prior.status === "credited" && next.status === "rejected") ||
      (prior.status === "rejected" && next.status === "credited")
    )
      fail();
    const rank: Record<string, number> = { pending: 0, credited: 1, rejected: 1, reversed: 2 };
    const status =
      (rank[prior.status] ?? 0) > (rank[next.status] ?? 0) ? prior.status : next.status;
    const amount = next.amount ?? prior.amount;
    merged = snapshot({
      ...next,
      currency: next.currency,
      amount,
      status,
      amount_refunded: Math.max(prior.amount_refunded, next.amount_refunded),
      amount_reversed:
        status === "reversed" && amount !== null
          ? amount
          : Math.max(prior.amount_reversed, next.amount_reversed),
    });
  }
  const contribution = depositNetAmount(merged);
  return Object.freeze({
    snapshot: merged,
    contribution,
    delta: contribution - (previous === null ? 0 : depositNetAmount(previous)),
  });
}
