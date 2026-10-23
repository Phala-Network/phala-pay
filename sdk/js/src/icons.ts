import { networkName, networkIconName } from "./chains.js";
import { ICON_IMAGES } from "./icon-images.js";

import { assetIconName, iconMonogram, type IconName } from "./icon-names.js";

function fallback(label: string): string {
  const letter = iconMonogram(label).replace(/[&<>"']/g, (character) => {
    switch (character) {
      case "&": return "&amp;";
      case "<": return "&lt;";
      case ">": return "&gt;";
      case '"': return "&quot;";
      default: return "&#39;";
    }
  });
  return `<svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="11" fill="currentColor" opacity="0.12"/><text x="12" y="16" text-anchor="middle" font-family="sans-serif" font-size="12" fill="currentColor">${letter}</text></svg>`;
}

/** CoinMarketCap's logo as a `data:` image, which needs `img-src data:` in a CSP. */
function logo(name: IconName): string {
  return `<svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" aria-hidden="true"><image href="${ICON_IMAGES[name]}" width="24" height="24"/></svg>`;
}

/** Decorative inline SVG markup; testnets reuse their mainnet family artwork. */
export function networkIcon(chainId: number): string {
  const name = networkIconName(chainId);
  return name === undefined ? fallback(networkName(chainId)) : logo(name);
}

/** Decorative inline SVG markup; keep the asset name visible beside it. */
export function assetIcon(asset: string): string {
  const name = assetIconName(asset);
  return name === undefined ? fallback(asset) : logo(name);
}
