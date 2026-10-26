import { useEffect, useState } from "react";
import type { WalletClient } from "viem";
import { watchWallets, type EthereumProvider, type Wallet } from "@phala/pay";

export interface WalletChoice {
  id: string;
  name: string;
  /** A data URI, or empty. */
  icon: string;
  wallet: WalletClient | EthereumProvider;
}

/** The wallets to pay with: the page's own `walletClient`, else the browser's as they announce themselves. */
export function useWallets(walletClient: WalletClient | undefined): WalletChoice[] {
  const [discovered, setDiscovered] = useState<Wallet[]>([]);
  useEffect(() => (walletClient === undefined ? watchWallets(setDiscovered) : undefined), [walletClient]);
  return walletClient === undefined
    ? discovered.map(({ info: { uuid, name, icon }, provider }) => ({ id: uuid, name, icon, wallet: provider }))
    : [{ id: "client", name: "your wallet", icon: "", wallet: walletClient }];
}
