import type { ICON_SVG } from "./icon-svg.js";

export type IconName = keyof typeof ICON_SVG;

/** Case-insensitive token symbol; unrecognized symbols use a neutral monogram. */
export function assetIconName(asset: string): IconName | undefined {
  switch (asset.trim().toLowerCase()) {
    case "usdc": return "usdc";
    case "usdt": return "usdt";
    case "pha": return "pha";
    case "eth": return "eth";
    default: return undefined;
  }
}

/** First character for a neutral fallback, including an empty-symbol placeholder. */
export function iconMonogram(label: string): string {
  return Array.from(label.trim())[0]?.toUpperCase() ?? "?";
}

