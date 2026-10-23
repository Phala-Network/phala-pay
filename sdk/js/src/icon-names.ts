import type { ICON_IMAGES } from "./icon-images.js";

export type IconName = keyof typeof ICON_IMAGES;

/** The tokens the SDK knows, by lowercase code: their icon and full name. */
const ASSETS: Readonly<Record<string, { icon: IconName; name: string }>> = {
  usdc: { icon: "usdc", name: "USD Coin" },
  usdt: { icon: "usdt", name: "Tether USD" },
  pha: { icon: "pha", name: "Phala Network" },
  eth: { icon: "eth", name: "Ether" },
};

function knownAsset(asset: string) {
  const code = asset.trim().toLowerCase();
  return Object.hasOwn(ASSETS, code) ? ASSETS[code] : undefined;
}

/** Case-insensitive token symbol; unrecognized symbols use a neutral monogram. */
export function assetIconName(asset: string): IconName | undefined {
  return knownAsset(asset)?.icon;
}

/** The token's symbol as payers read it, for example `USDC` for `usdc`. */
export function assetSymbol(asset: string): string {
  return asset.trim().toUpperCase();
}

/** The token's full name, for example `USD Coin`, or its symbol when the SDK does not know it. */
export function assetName(asset: string): string {
  return knownAsset(asset)?.name ?? assetSymbol(asset);
}

/** First character for a neutral fallback, including an empty-symbol placeholder. */
export function iconMonogram(label: string): string {
  return Array.from(label.trim())[0]?.toUpperCase() ?? "?";
}
