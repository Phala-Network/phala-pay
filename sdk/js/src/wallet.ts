import { submitWalletTransaction } from "./transactions.js";
import { createStore } from "mipd";
import {
  BaseError,
  createWalletClient,
  custom,
  erc20Abi,
  SwitchChainError,
  UserRejectedRequestError,
  type Address,
  type Hash,
  type WalletClient,
} from "viem";
import { readContract } from "viem/actions";
import { knownChain, networkName } from "./chains.js";
import { formatTokenAmount, formatUnitsGrouped } from "./format.js";
import { quoteTransfer } from "./payment.js";
import type { ClientQuote } from "./quote.js";

/** EIP-6963 provider info. */
export interface WalletInfo {
  uuid: string;
  name: string;
  /** A data URI. */
  icon: string;
  rdns: string;
}

/** An EIP-1193 provider; viem's `EIP1193Provider` and every injected wallet satisfy it. */
export interface EthereumProvider {
  request(args: { method: string; params?: unknown }): Promise<unknown>;
}

export interface Wallet {
  info: WalletInfo;
  provider: EthereumProvider;
}

/** The legacy `window.ethereum` provider, offered when no wallet announces itself (EIP-6963). */
export const INJECTED_WALLET_UUID = "injected";

/**
 * Discovers browser wallets with EIP-6963 (through mipd's store), falling back to
 * `window.ethereum`, and calls `onChange` with the current list whenever a wallet announces
 * itself. Returns the function that stops listening.
 */
export function watchWallets(onChange: (wallets: Wallet[]) => void): () => void {
  if (typeof window === "undefined") {
    onChange([]);
    return () => undefined;
  }
  const store = createStore();
  const unsubscribe = store.subscribe((details) => onChange(walletsFrom(details)), {
    emitImmediately: true,
  });
  return () => {
    unsubscribe();
    store.destroy();
  };
}

type Announcement = { info?: Partial<WalletInfo>; provider?: unknown };

/** Announcements are untrusted page events: keep well-formed ones, and only data-URI icons. */
function walletsFrom(announcements: readonly Announcement[]): Wallet[] {
  const wallets: Wallet[] = [];
  for (const { info, provider } of announcements) {
    if (!isProvider(provider) || typeof info?.uuid !== "string" || typeof info.name !== "string") {
      continue;
    }
    const icon = typeof info.icon === "string" && info.icon.startsWith("data:image/") ? info.icon : "";
    const rdns = typeof info.rdns === "string" ? info.rdns : "";
    wallets.push({ info: { uuid: info.uuid, name: info.name, icon, rdns }, provider });
  }
  // Wallets that predate EIP-6963, such as some in-app mobile browsers, only set `window.ethereum`.
  const injected = (window as { ethereum?: EthereumProvider }).ethereum;
  if (wallets.length === 0 && injected !== undefined) {
    wallets.push({
      info: { uuid: INJECTED_WALLET_UUID, name: "Browser wallet", icon: "", rdns: "" },
      provider: injected,
    });
  }
  return wallets;
}

function isProvider(value: unknown): value is EthereumProvider {
  return typeof (value as Partial<EthereumProvider> | null | undefined)?.request === "function";
}

export type WalletErrorCode =
  | "rejected"
  | "no_account"
  | "wrong_chain"
  | "insufficient_balance"
  | "failed";

export class WalletError extends Error {
  override readonly name = "WalletError";

  constructor(
    readonly code: WalletErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options);
  }
}

/**
 * Pays a quote from a wallet: a viem `WalletClient` (for example wagmi's `useWalletClient()`), or an
 * EIP-1193 provider such as a discovered `Wallet`'s. Connects (unless the client already has an
 * account), switches to (or adds) the quote's chain, and sends the ERC-20 `transfer` that the quote's
 * `payment_uri` states. Before sending, it reads the account's token balance and, when it is below
 * the quote's amount, sends nothing and throws `WalletError` `insufficient_balance`: the transfer
 * would revert and still cost gas. Resolves with the transaction hash once the wallet has broadcast
 * it; the checkout's status follows the payment from there.
 */
export async function payWithWallet(
  wallet: WalletClient | EthereumProvider,
  quote: ClientQuote,
): Promise<Hash> {
  const transfer = quoteTransfer(quote);
  const client = isWalletClient(wallet) ? wallet : createWalletClient({ transport: custom(wallet) });
  try {
    const account = client.account ?? (await client.requestAddresses())[0];
    if (account === undefined) {
      throw new WalletError("no_account", "The wallet shared no account");
    }
    await ensureChain(client, transfer.chainId);
    const address = typeof account === "string" ? account : account.address;
    const balance = await tokenBalance(client, transfer.token, address);
    if (balance !== undefined && balance < transfer.amount) {
      const symbol = quote.asset.toUpperCase();
      throw new WalletError(
        "insufficient_balance",
        `Your wallet holds ${formatUnitsGrouped(balance, quote.decimals)} ${symbol}, less than the ` +
          `${formatTokenAmount(quote)} ${symbol} to pay. Nothing was sent.`,
      );
    }
    const hash = await client.writeContract({
      account,
      chain: knownChain(transfer.chainId) ?? null,
      address: transfer.token,
      abi: erc20Abi,
      functionName: "transfer",
      args: [transfer.to, transfer.amount],
    });
    submitWalletTransaction(quote, hash);
    return hash;
  } catch (error) {
    if (error instanceof WalletError) {
      throw error;
    }
    if (isRejection(error)) {
      throw new WalletError("rejected", "The request was rejected in the wallet", { cause: error });
    }
    throw new WalletError("failed", "The wallet could not send the payment", { cause: error });
  }
}

/**
 * The account's balance of `token`, read through the wallet on its current chain; `undefined` when
 * the wallet cannot read it. The check only spares the payer a reverting transfer, so a wallet that
 * does not serve `eth_call` still pays.
 */
async function tokenBalance(wallet: WalletClient, token: Address, owner: Address): Promise<bigint | undefined> {
  try {
    return await readContract(wallet, { address: token, abi: erc20Abi, functionName: "balanceOf", args: [owner] });
  } catch {
    return undefined;
  }
}

function isWalletClient(wallet: WalletClient | EthereumProvider): wallet is WalletClient {
  return "writeContract" in wallet;
}

async function ensureChain(wallet: WalletClient, chainId: number): Promise<void> {
  if ((await wallet.getChainId()) === chainId) {
    return;
  }
  try {
    await wallet.switchChain({ id: chainId });
  } catch (error) {
    const chain = knownChain(chainId);
    if (!isUnknownChain(error) || chain === undefined) {
      throw isRejection(error)
        ? error
        : new WalletError("wrong_chain", `Switch your wallet to ${networkName(chainId)}`, {
            cause: error,
          });
    }
    await wallet.addChain({ chain });
    if ((await wallet.getChainId()) !== chainId) {
      await wallet.switchChain({ id: chainId });
    }
  }
  if ((await wallet.getChainId()) !== chainId) {
    throw new WalletError("wrong_chain", `Switch your wallet to ${networkName(chainId)}`);
  }
}

function isRejection(error: unknown): boolean {
  return error instanceof BaseError && error.walk((e) => e instanceof UserRejectedRequestError) !== null;
}

/** Whether the wallet does not know the chain (4902). MetaMask Mobile nests that code in an internal
 * error's `data.originalError`, which wagmi's injected connector also unwraps. */
function isUnknownChain(error: unknown): boolean {
  return (
    error instanceof BaseError &&
    error.walk((e) => {
      const nested = (e as { data?: { originalError?: { code?: unknown } } } | null)?.data;
      return e instanceof SwitchChainError || nested?.originalError?.code === SwitchChainError.code;
    }) !== null
  );
}
