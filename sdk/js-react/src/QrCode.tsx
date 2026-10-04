import { useMemo } from "react";
import { encode } from "uqr";

export interface QrCodeProps {
  value: string;
  label: string;
  size?: number;
}

/** A QR code as an SVG path, one unit per module. */
export function QrCode({ value, label, size = 208 }: QrCodeProps) {
  const { path, width } = useMemo(() => {
    const { data, size: modules } = encode(value, { ecc: "M", border: 2 });
    let d = "";
    data.forEach((row, y) => {
      row.forEach((dark, x) => {
        if (dark) {
          d += `M${x} ${y}h1v1h-1z`;
        }
      });
    });
    return { path: d, width: modules };
  }, [value]);

  return (
    <svg
      className="pp-qr"
      role="img"
      aria-label={label}
      width={size}
      height={size}
      viewBox={`0 0 ${width} ${width}`}
      shapeRendering="crispEdges"
    >
      <rect width={width} height={width} fill="#fff" />
      <path d={path} fill="#000" />
    </svg>
  );
}
