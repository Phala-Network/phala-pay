import { WalletError, knownChain, networkName, watchWallets, type EthereumProvider, type Wallet } from "@phala/pay";
import {
  createWalletClient,
  custom,
  erc20Abi,
  getAddress,
  isHex,
  parseAbi,
  type Account,
  type Address,
  type Chain,
  type Hash,
  type ReplacementReason,
  type WalletClient,
} from "viem";
import { readContract, waitForTransactionReceipt } from "viem/actions";
import { tokens } from "./format.js";

// Staging's Sepolia test PHA is a MockERC20 whose `mint(address,uint256)` is public, and its
// Sepolia test USDT is Aave's, which Aave's faucet contract mints to anyone with
// `mint(token, to, amount)`: the visitor's own wallet mints either, so the demo holds no faucet key.
// Gas is the testnet's ETH from any public faucet. The same wallet pays to the deposit address,
// sends a sweep (a flush is permissionless), and may pay a refund, which fails verification unless
// it comes from the treasury.
const MINT_ABI = parseAbi(["function mint(address account, uint256 amount)"]);
const FAUCET_MINT_ABI = parseAbi(["function mint(address token, address to, uint256 amount) returns (uint256)"]);

/** The first wallet the browser announces within 300 ms (EIP-6963, or `window.ethereum`). */
export function firstWallet(): Promise<Wallet | undefined> {
  return new Promise((resolve) => {
    let found: Wallet | undefined;
    const stop = watchWallets((wallets) => {
      found ??= wallets[0];
    });
    setTimeout(() => {
      stop();
      resolve(found);
    }, 300);
  });
}

interface Connected {
  client: WalletClient;
  account: Account | Address;
  chain: Chain;
}

/**
 * A wallet a payment used, as the SDK's checkout reports it with a `WalletError`: the chosen browser
 * wallet's provider, or a page's own viem client.
 */
export type PaidWith = WalletClient | EthereumProvider;

/**
 * `using` (or else the first browser wallet), its account (the client's own, or the wallet's first,
 * as the SDK pays with), switched to `chainId`.
 */
async function connect(chainId: number, using?: PaidWith): Promise<Connected> {
  const chain = knownChain(chainId);
  if (chain === undefined) {
    throw new Error(`Unsupported network ${networkName(chainId)}`);
  }
  let client: WalletClient;
  if (using !== undefined && "writeContract" in using) {
    client = using;
  } else {
    const provider = using ?? (await firstWallet())?.provider;
    if (provider === undefined) {
      throw new Error("No browser wallet found.");
    }
    client = createWalletClient({ transport: custom(provider) });
  }
  const account = client.account ?? (await client.requestAddresses())[0];
  if (account === undefined) {
    throw new Error("The wallet shared no account");
  }
  if ((await client.getChainId()) !== chainId) {
    try {
      await client.switchChain({ id: chainId });
    } catch {
      await client.addChain({ chain });
      await client.switchChain({ id: chainId });
    }
  }
  return { client, account, chain };
}

/**
 * Mints `amountAtomic` of a test token to the visitor's wallet (`using`, else the first browser
 * wallet), through its `minter` faucet contract when it has one; resolves with the mint's
 * transaction once it is in a block, the repriced one if the wallet sped it up. A mint the wallet
 * canceled or replaced with another call minted nothing.
 */
export async function mintTestTokens(
  chainId: number,
  token: { contract: string; minter: string | null },
  amountAtomic: bigint,
  using?: PaidWith,
): Promise<Hash> {
  const { client, account, chain } = await connect(chainId, using);
  const to = addressOf(account);
  const { minter } = token;
  const hash =
    minter === null
      ? await client.writeContract({
          account,
          chain,
          address: getAddress(token.contract),
          abi: MINT_ABI,
          functionName: "mint",
          args: [to, amountAtomic],
        })
      : await client.writeContract({
          account,
          chain,
          address: getAddress(minter),
          abi: FAUCET_MINT_ABI,
          functionName: "mint",
          args: [getAddress(token.contract), to, amountAtomic],
        });
  // So that a payment right after it sees the minted balance.
  let replaced: ReplacementReason | undefined;
  const receipt = await waitForTransactionReceipt(client, {
    hash,
    onReplaced: ({ reason }) => {
      replaced = reason;
    },
  });
  if (replaced === "cancelled") {
    throw new Error("The mint was canceled in the wallet; nothing was minted.");
  }
  if (replaced === "replaced") {
    throw new Error("The wallet replaced the mint with another transaction; nothing was minted.");
  }
  if (receipt.status !== "success") {
    throw new Error("The mint transaction failed.");
  }
  return receipt.transactionHash;
}

/**
 * An ERC-20 transfer from the visitor's wallet. A wallet holding less than the amount sends
 * nothing (the transfer would revert and still cost gas): `WalletError` `insufficient_balance`, as
 * the SDK's checkout refuses it.
 */
export async function transferTokens(
  chainId: number,
  token: { contract: string; symbol: string; decimals: number },
  to: string,
  amountAtomic: bigint,
): Promise<Hash> {
  const { client, account, chain } = await connect(chainId);
  const contract = getAddress(token.contract);
  const balance = await readContract(client, {
    address: contract,
    abi: erc20Abi,
    functionName: "balanceOf",
    args: [addressOf(account)],
  });
  if (balance < amountAtomic) {
    const held = tokens(balance.toString(), token.symbol, token.decimals);
    const needed = tokens(amountAtomic.toString(), token.symbol, token.decimals);
    throw new WalletError("insufficient_balance", `Your wallet holds ${held}, less than the ${needed} to send. Nothing was sent.`);
  }
  return client.writeContract({
    account,
    chain,
    address: contract,
    abi: erc20Abi,
    functionName: "transfer",
    args: [getAddress(to), amountAtomic],
  });
}

function addressOf(account: Account | Address): Address {
  return typeof account === "string" ? account : account.address;
}

/** Sends a prepared call, such as the SDK's `factory.flush(treasury, salts, token)`. */
export async function sendCall(chainId: number, call: { to: string; data: string; value: string }): Promise<Hash> {
  if (!isHex(call.data)) {
    throw new Error("The call's data is not hex");
  }
  const { client, account, chain } = await connect(chainId);
  return client.sendTransaction({
    account,
    chain,
    to: getAddress(call.to),
    data: call.data,
    value: BigInt(call.value),
  });
}
