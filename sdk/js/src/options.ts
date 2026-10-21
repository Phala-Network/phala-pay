import { networkName } from "./chains.js";
import { assetSymbol } from "./icon-names.js";

/** A token on a network that a payer can pay with, for example `usdc` on chain 8453. */
export interface PaymentOption {
  chain_id: number;
  /** The token's code, for example `usdc`. */
  asset: string;
}

/** A network with the tokens it takes, as a deposit address's `networks` list them. */
export interface PaymentNetwork {
  chain_id: number;
  assets: readonly { asset: string }[];
}

/** One token of a `PaymentNetwork`, with the network and token objects it came from. */
export type NetworkPaymentOption<N extends PaymentNetwork> = PaymentOption & {
  network: N;
  token: N["assets"][number];
};

/**
 * The options to offer, in the order given and each once (the first of any duplicate): either a
 * flat list such as `GET /v1/config`'s `assets`, returned as the same objects, or networks with
 * their tokens such as a deposit address's `networks`, flattened network by network.
 */
export function paymentOptions<T extends PaymentOption>(options: readonly T[]): T[];
export function paymentOptions<N extends PaymentNetwork>(networks: readonly N[]): NetworkPaymentOption<N>[];
export function paymentOptions(
  source: readonly (PaymentOption | PaymentNetwork)[],
): (PaymentOption | NetworkPaymentOption<PaymentNetwork>)[] {
  const flat = source.flatMap((each) =>
    "assets" in each
      ? each.assets.map((token) => ({ chain_id: each.chain_id, asset: token.asset, network: each, token }))
      : [each],
  );
  return flat.filter((option, index) => flat.findIndex((other) => samePaymentOption(option, other)) === index);
}

/** Whether `a` and `b` are the same token on the same network; asset codes are case-insensitive. */
export function samePaymentOption(a: PaymentOption | null | undefined, b: PaymentOption | null | undefined): boolean {
  return (
    a != null &&
    b != null &&
    a.chain_id === b.chain_id &&
    a.asset.trim().toLowerCase() === b.asset.trim().toLowerCase()
  );
}

/** The option as one line of text, for example `USDC · Ethereum`. */
export function paymentOptionLabel(option: PaymentOption): string {
  return `${assetSymbol(option.asset)} · ${networkName(option.chain_id)}`;
}
