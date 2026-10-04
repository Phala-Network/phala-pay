import { useEffect, useState } from "react";

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
        <span className={copy === false ? undefined : "pp-value"}>{value}</span>
        {copy !== false && (
          <CopyButton value={typeof copy === "string" ? copy : value} label={label} />
        )}
      </dd>
    </div>
  );
}

function CopyButton({ value, label }: { value: string; label: string }) {
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
    <button type="button" className="pp-copy" onClick={onClick} aria-label={`Copy ${label}`}>
      <span aria-live="polite">{copied === null ? "Copy" : copied ? "Copied" : "Copy failed"}</span>
    </button>
  );
}
