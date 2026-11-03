import { mainnet, sepolia, base, baseSepolia } from "viem/chains";

// The same viem chains as @phala/pay, checked against the browser registry in tests.
const chains = [mainnet, sepolia, base, baseSepolia];
export function networkName(chainId: number): string {
  return chains.find((chain) => chain.id === chainId)?.name ?? `Chain ${chainId}`;
}

/** Explorer URL, or undefined for an unknown chain or malformed transaction hash. */
export function transactionUrl(chainId: number, txHash: string): string | undefined {
  if (txHash.length !== 66 || !/^0x[0-9a-fA-F]{64}$/.test(txHash)) return undefined;
  const explorer = chains.find((chain) => chain.id === chainId)?.blockExplorers.default.url;
  return explorer === undefined ? undefined : `${explorer}/tx/${txHash}`;
}

/** Explorer URL, or undefined for an unknown chain or malformed address. */
export function addressUrl(chainId: number, address: string): string | undefined {
  if (address.length !== 42 || !/^0x[0-9a-fA-F]{40}$/.test(address)) return undefined;
  const explorer = chains.find((chain) => chain.id === chainId)?.blockExplorers.default.url;
  return explorer === undefined ? undefined : `${explorer}/address/${address}`;
}
