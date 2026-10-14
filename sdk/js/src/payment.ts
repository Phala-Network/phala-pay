import { getAddress, isAddressEqual, type Address } from "viem";
import type { ClientQuote } from "./quote.js";

/** An ERC-20 transfer request, as EIP-681 encodes it and a wallet sends it. */
export interface TokenTransfer {
  chainId: number;
  /** The token contract. */
  token: Address;
  /** The recipient: the quote's or the customer's deposit address. */
  to: Address;
  /** Token amount in the smallest unit. */
  amount: bigint;
}

/** An EIP-681 ERC-20 transfer request whose amount the payer may choose. */
export type TransferRequest = Omit<TokenTransfer, "amount"> & { amount: bigint | undefined };

/** A token a deposit address takes on one network. */
export interface DepositAddressAsset {
  /** The token's code, for example `pha`. */
  asset: string;
  /** The ERC-20 contract. */
  contract: string;
  decimals: number;
  /** Minimum per-transfer credit in account currency minor units (USD cents), independent of atomic bounds; absent on older servers and admin restore. */
  min_amount?: number;
  /** Inclusive per-transfer token minimum; zero does not waive min_amount. Absent on older servers and admin restore. */
  min_deposit_atomic?: string;
  /** Inclusive per-transfer maximum at view time, in atomic units; absent on older servers and admin restore. */
  max_deposit_atomic?: string;
  /** The EIP-681 transfer request of this token to the network's address, without an amount. */
  payment_uri: string;
}

/** A deposit address on one network (EVM chain), with the tokens it takes there. */
export interface DepositAddressNetwork {
  chain_id: number;
  /** The address on this chain: the same on every chain whose treasury is the same. */
  address: string;
  assets: DepositAddressAsset[];
}

/**
 * The fields of a deposit address that a payer's page shows: pass them from your backend's
 * `POST /v1/deposit_addresses` response. One address takes every supported token on every
 * supported network; eligible transfers within its deposit limits are credited at the market rate.
 */
export interface DepositAddressDetails {
  /** The address shared by every network, or `null` when a network's address differs. */
  address: string | null;
  networks: DepositAddressNetwork[];
}

const EIP681_TRANSFER =
  /^ethereum:(?:pay-)?(0x[0-9a-fA-F]{40})@([1-9][0-9]*)\/transfer\?([^#]*)$/;

/**
 * Parses an EIP-681 ERC-20 transfer URI, `ethereum:<token>@<chainId>/transfer?address=<to>`, with
 * an optional `uint256=<amount>`. Throws on anything else.
 */
export function parseTransferUri(uri: string): TransferRequest {
  const match = EIP681_TRANSFER.exec(uri);
  const params = new URLSearchParams(match?.[3] ?? "");
  const to = params.get("address");
  const amount = params.get("uint256");
  if (
    match?.[1] === undefined ||
    match[2] === undefined ||
    to === null ||
    !/^0x[0-9a-fA-F]{40}$/.test(to) ||
    (amount !== null && !/^\d+$/.test(amount))
  ) {
    throw new TypeError("payment_uri is not an EIP-681 ERC-20 transfer");
  }
  return {
    chainId: Number(match[2]),
    token: getAddress(match[1]),
    to: getAddress(to),
    amount: amount === null ? undefined : BigInt(amount),
  };
}

/**
 * Reads the token transfer from a quote's EIP-681 `payment_uri`, and checks that it pays exactly
 * the quote's `amount_atomic` to the quote's `address` on the quote's chain, so the wallet, the QR
 * code, and the manual instructions all state the same payment.
 */
export function quoteTransfer(quote: ClientQuote): TokenTransfer {
  const transfer = parseTransferUri(quote.payment_uri);
  if (
    transfer.amount === undefined ||
    transfer.chainId !== quote.chain_id ||
    !isAddressEqual(transfer.to, getAddress(quote.address)) ||
    transfer.amount !== BigInt(quote.amount_atomic)
  ) {
    throw new TypeError("payment_uri does not match the quote");
  }
  return { ...transfer, amount: transfer.amount };
}

/**
 * Reads the token transfer from a deposit address's EIP-681 `payment_uri` for one token on one
 * network, and checks that it pays the network's address in that token on that chain and names no
 * amount, so the QR code and the copied address and contract state the same destination.
 */
export function depositAddressTransfer(
  network: DepositAddressNetwork,
  asset: DepositAddressAsset,
): TransferRequest {
  const transfer = parseTransferUri(asset.payment_uri);
  if (
    transfer.amount !== undefined ||
    transfer.chainId !== network.chain_id ||
    !isAddressEqual(transfer.to, getAddress(network.address)) ||
    !isAddressEqual(transfer.token, getAddress(asset.contract))
  ) {
    throw new TypeError("payment_uri does not match the deposit address");
  }
  return transfer;
}
