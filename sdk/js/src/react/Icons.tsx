"use client";

import { networkName, networkIconName } from "../chains.js";
import { assetIconName, iconMonogram, type IconName } from "../icon-names.js";
import { ICON_PATHS } from "./icon-paths.js";

export interface IconProps {
  /** Width and height in pixels; default 18. No inline styles are injected. */
  size?: number;
  /** Default true next to visible text. Set false when the icon is used alone. */
  decorative?: boolean;
}
export interface NetworkIconProps extends IconProps { chainId: number }
export interface AssetIconProps extends IconProps { asset: string }

function Icon({ name, label, size = 18, decorative = true }: IconProps & {
  name: IconName | undefined;
  label: string;
}) {
  const pixels = Number.isFinite(size) && size > 0 ? size : 18;
  return (
    <span className="pp-icon-label" role={decorative ? undefined : "img"}
      aria-label={decorative ? undefined : label || "Unknown asset"}>
      <svg className="pp-icon" xmlns="http://www.w3.org/2000/svg" width={pixels} height={pixels}
        viewBox="0 0 24 24" fill="none" aria-hidden="true" focusable="false">
        {name === undefined ? <>
          <circle cx="12" cy="12" r="11" fill="currentColor" opacity="0.12" />
          <text x="12" y="16" textAnchor="middle" fontFamily="sans-serif" fontSize="12" fill="currentColor">{iconMonogram(label)}</text>
        </> : ICON_PATHS[name]}
      </svg>
    </span>
  );
}

/** Branded network icon; testnets reuse their mainnet family artwork. */
export function NetworkIcon({ chainId, ...props }: NetworkIconProps) {
  return <Icon {...props} name={networkIconName(chainId)} label={networkName(chainId)} />;
}

/** Branded token icon, or a neutral first-letter monogram for unknown symbols. */
export function AssetIcon({ asset, ...props }: AssetIconProps) {
  return <Icon {...props} name={assetIconName(asset)} label={asset.trim().toUpperCase()} />;
}
