import { Fragment, useEffect, useState } from "react";
import { CheckGlyph, CopyGlyph, CrossGlyph } from "./Glyphs.js";

/** `copy` adds a copy button, copying `value`, or the given string when it differs from it. */
export function Field({
  label,
  value,
  copy = false,
}: {
  label: string;
  value: string;
  copy?: boolean | string;
}) {
  return (
    <div className="pp-field">
      <dt>{label}</dt>
      <dd>
        <span className={copy === false ? undefined : "pp-value"}><Grouped value={value} /></span>
        {copy !== false && (
          <CopyButton value={typeof copy === "string" ? copy : value} label={label} />
        )}
      </dd>
    </div>
  );
}

/**
 * A `0x` hex value in full, in groups of four characters after `0x`, so that a payer can compare
 * it group by group; never shortened. It wraps only between groups, and selecting and copying it
 * gives the value without spaces. Any other value is shown as it is.
 */
function Grouped({ value }: { value: string }) {
  if (!/^0x[0-9a-fA-F]{8,}$/.test(value)) {
    return <>{value}</>;
  }
  const groups = [value.slice(0, 6)];
  for (let start = 6; start < value.length; start += 4) {
    groups.push(value.slice(start, start + 4));
  }
  return groups.map((group, index) => (
    <Fragment key={index}>
      {index > 0 && <wbr />}
      <span className="pp-group">{group}</span>
    </Fragment>
  ));
}

export function CopyButton({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState<boolean | null>(null);
  useEffect(() => {
    if (copied === null) {
      return;
    }
    const timer = setTimeout(() => setCopied(null), 2000);
    return () => clearTimeout(timer);
  }, [copied]);
  const onClick = () => {
    navigator.clipboard.writeText(value).then(
      () => setCopied(true),
      () => setCopied(false),
    );
  };
  return (
    <button
      type="button"
      className="pp-copy"
      onClick={onClick}
      aria-label={`Copy ${label}`}
      data-state={copied === null ? undefined : copied ? "copied" : "failed"}
    >
      {copied === null ? <CopyGlyph /> : copied ? <CheckGlyph /> : <CrossGlyph />}
      <span className="pp-visually-hidden" aria-live="polite">
        {copied === null ? "Copy" : copied ? "Copied" : "Copy failed"}
      </span>
    </button>
  );
}
