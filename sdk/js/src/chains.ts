import type { Chain } from "viem";
import { base, baseSepolia, mainnet, sepolia } from "viem/chains";

/** Chains a wallet can be asked to add; any other chain must already be in the wallet. */
const CHAINS: readonly { chain: Chain; icon: "ethereum" | "base" }[] = [
  { chain: mainnet, icon: "ethereum" },
  { chain: sepolia, icon: "ethereum" },
  { chain: base, icon: "base" },
  { chain: baseSepolia, icon: "base" },
];

/** Icon family stored with the chain registry, including testnets. */
export function networkIconName(chainId: number): "ethereum" | "base" | undefined {
  return CHAINS.find(({ chain }) => chain.id === chainId)?.icon;
}

export function knownChain(chainId: number): Chain | undefined {
  return CHAINS.find(({ chain }) => chain.id === chainId)?.chain;
}

/** A human name for the network, for example `Sepolia`. */
export function networkName(chainId: number): string {
  return knownChain(chainId)?.name ?? `Chain ${chainId}`;
}

/** The block explorer link for a transaction, when the chain is known. */
export function transactionUrl(chainId: number, hash: string): string | undefined {
  const explorer = knownChain(chainId)?.blockExplorers?.default.url;
  return explorer === undefined ? undefined : `${explorer}/tx/${hash}`;
}
