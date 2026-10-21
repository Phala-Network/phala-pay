import type { Asset, Network } from "./api.js";

/** The network with `chainId`, else the first: a payment from before rows named their chain. */
export function networkOf(networks: Network[] | undefined, chainId: number | undefined): Network | undefined {
  return networks?.find((network) => network.chain_id === chainId) ?? networks?.[0];
}

/** The token `asset` on the network, for its symbol and decimals. */
export function assetOf(network: Network | undefined, asset: string | null | undefined): Asset | undefined {
  return network?.assets.find((each) => each.asset === asset?.toLowerCase());
}
