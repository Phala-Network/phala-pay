import { mainnet, sepolia, base, baseSepolia } from "viem/chains";

// The same viem chains as @phala/pay, checked against the browser registry in tests.
const chains = [mainnet, sepolia, base, baseSepolia];
export function networkName(chainId: number): string {
  return chains.find((chain) => chain.id === chainId)?.name ?? `Chain ${chainId}`;
}

/** Transaction explorer URL, or undefined for an unknown chain. */
export function transactionUrl(chainId: number, txHash: string): string | undefined {
  const explorer = chains.find((chain) => chain.id === chainId)?.blockExplorers.default.url;
  return explorer === undefined ? undefined : `${explorer}/tx/${txHash}`;
}

/** Address/token-contract explorer URL, or undefined for an unknown chain. */
export function addressUrl(chainId: number, address: string): string | undefined {
  const explorer = chains.find((chain) => chain.id === chainId)?.blockExplorers.default.url;
  return explorer === undefined ? undefined : `${explorer}/address/${address}`;
}
